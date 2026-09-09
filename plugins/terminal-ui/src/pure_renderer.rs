use super::*;
use brood_plugin_sdk::{Facets, facet_codec, facets};
use std::collections::{HashMap, HashSet};

const CURSOR_BLINK_NS: u64 = CURSOR_BLINK_MS * 1_000_000;
const VIEW_MAGIC: &[u8; 4] = b"BTVW";
const INTERACTION_MAGIC: &[u8; 4] = b"BTIM";
const FACET_CODEC_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq)]
struct View {
    snapshot: Option<Snapshot>,
    oversized: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Sequence {
    selection: Option<Selection>,
    pressed_mouse_button: Option<u8>,
    last_pointer: Option<CellPoint>,
    mouse_modifiers: u32,
    scroll_accumulator: f32,
    mouse_wheel_accumulator: f32,
    suppressed_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Interaction {
    revision: u64,
    rows: u16,
    cols: u16,
    cell_width: f32,
    line_height: f32,
    application_cursor: bool,
    bracketed_paste: bool,
    mouse_tracking: u8,
    mouse_sgr: bool,
    viewport_offset: usize,
    max_viewport_offset: usize,
    rows_data: Vec<Vec<Run>>,
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn style_bits(run: &Run) -> u8 {
    u8::from(run.bold)
        | (u8::from(run.faint) << 1)
        | (u8::from(run.italic) << 2)
        | (u8::from(run.underline) << 3)
        | (u8::from(run.strikethrough) << 4)
}

fn layout_unit_count(snapshot: &Snapshot) -> Option<usize> {
    let mut units = 0_usize;
    for (row, runs) in snapshot.rows_data.iter().enumerate() {
        for run in runs {
            if row as u16 != snapshot.cursor_row {
                if !run.text.trim_end().is_empty() {
                    units = units.checked_add(1)?;
                }
                continue;
            }
            units = units.checked_add(
                terminal_text_clusters(&run.text)
                    .iter()
                    .filter(|(_, _, text)| text != " ")
                    .count(),
            )?;
        }
    }
    Some(units)
}

fn layout_key_count(snapshot: &Snapshot) -> Option<usize> {
    let mut keys = HashSet::new();
    for (row, runs) in snapshot.rows_data.iter().enumerate() {
        for run in runs {
            if row as u16 == snapshot.cursor_row {
                for (_, cells, text) in terminal_text_clusters(&run.text) {
                    if text != " " {
                        keys.insert(RunKey {
                            cells,
                            fg: run.fg,
                            bold: run.bold,
                            faint: run.faint,
                            italic: run.italic,
                            text,
                        });
                    }
                }
            } else {
                let text = run.text.trim_end();
                if text.is_empty() {
                    continue;
                }
                let cells = text
                    .chars()
                    .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
                    .sum::<usize>()
                    .max(1)
                    .min(usize::from(run.cells)) as u16;
                keys.insert(RunKey {
                    cells,
                    fg: run.fg,
                    bold: run.bold,
                    faint: run.faint,
                    italic: run.italic,
                    text: text.to_owned(),
                });
            }
        }
    }
    Some(keys.len())
}

fn encode_rows(bytes: &mut Vec<u8>, rows: &[Vec<Run>], cols: u16) -> Result<(), ()> {
    let mut run_total = 0_usize;
    for runs in rows {
        let run_count = u16::try_from(runs.len()).map_err(|_| ())?;
        run_total = run_total.checked_add(runs.len()).ok_or(())?;
        if run_total > MAX_SNAPSHOT_RUNS {
            return Err(());
        }
        push_u16(bytes, run_count);
        for run in runs {
            let text_len = u16::try_from(run.text.len()).map_err(|_| ())?;
            if run.cells == 0 || run.start.saturating_add(run.cells) > cols {
                return Err(());
            }
            push_u16(bytes, run.start);
            push_u16(bytes, run.cells);
            push_u32(bytes, run.fg);
            push_u32(bytes, run.bg);
            bytes.push(style_bits(run));
            push_u16(bytes, text_len);
            bytes.extend_from_slice(run.text.as_bytes());
            if bytes.len() > MAX_SNAPSHOT_ENCODED_BYTES {
                return Err(());
            }
        }
    }
    Ok(())
}

fn decode_rows(reader: &mut Reader<'_>, row_count: u16, cols: u16) -> Result<Vec<Vec<Run>>, ()> {
    let mut rows = Vec::with_capacity(usize::from(row_count));
    let mut run_total = 0_usize;
    for _ in 0..row_count {
        let count = reader.u16()?;
        run_total = run_total.checked_add(usize::from(count)).ok_or(())?;
        if run_total > MAX_SNAPSHOT_RUNS {
            return Err(());
        }
        let mut runs = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let start = reader.u16()?;
            let cells = reader.u16()?;
            let fg = reader.u32()?;
            let bg = reader.u32()?;
            let style = reader.u8()?;
            let text_len = reader.u16()?;
            let text = std::str::from_utf8(reader.take(usize::from(text_len))?)
                .map_err(|_| ())?
                .to_owned();
            if cells == 0 || start.saturating_add(cells) > cols {
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
        rows.push(runs);
    }
    Ok(rows)
}

fn screen_bits(snapshot: &Snapshot) -> u8 {
    u8::from(snapshot.cursor_visible)
        | (u8::from(snapshot.application_cursor) << 1)
        | (u8::from(snapshot.bracketed_paste) << 2)
        | ((snapshot.mouse_tracking & 3) << 3)
        | (u8::from(snapshot.mouse_sgr) << 5)
        | (u8::from(snapshot.exited) << 6)
}

fn encode_snapshot(bytes: &mut Vec<u8>, snapshot: &Snapshot) -> Result<(), ()> {
    if snapshot.rows == 0
        || snapshot.rows > MAX_SNAPSHOT_ROWS
        || snapshot.cols == 0
        || snapshot.cols > MAX_SNAPSHOT_COLS
        || snapshot.rows_data.len() != usize::from(snapshot.rows)
        || snapshot.viewport_offset > snapshot.max_viewport_offset
        || layout_unit_count(snapshot).is_none_or(|units| units > MAX_SNAPSHOT_LAYOUT_UNITS)
        || layout_key_count(snapshot).is_none_or(|keys| keys > MAX_SNAPSHOT_LAYOUT_UNITS)
    {
        return Err(());
    }
    push_u64(bytes, snapshot.revision);
    push_u64(bytes, snapshot.input_sequence);
    push_u16(bytes, snapshot.rows);
    push_u16(bytes, snapshot.cols);
    push_u16(bytes, snapshot.cursor_row);
    push_u16(bytes, snapshot.cursor_col);
    bytes.push(screen_bits(snapshot));
    bytes.extend_from_slice(&snapshot.exit_code.unwrap_or(i32::MIN).to_le_bytes());
    push_u64(bytes, snapshot.chunks_read);
    push_u64(bytes, snapshot.bytes_read);
    push_u64(bytes, snapshot.resizes);
    push_u32(
        bytes,
        u32::try_from(snapshot.viewport_offset).map_err(|_| ())?,
    );
    push_u32(
        bytes,
        u32::try_from(snapshot.max_viewport_offset).map_err(|_| ())?,
    );
    encode_rows(bytes, &snapshot.rows_data, snapshot.cols)
}

fn decode_snapshot(reader: &mut Reader<'_>) -> Result<Snapshot, ()> {
    let revision = reader.u64()?;
    let input_sequence = reader.u64()?;
    let rows = reader.u16()?;
    let cols = reader.u16()?;
    let cursor_row = reader.u16()?;
    let cursor_col = reader.u16()?;
    let flags = reader.u8()?;
    let raw_exit = reader.i32()?;
    let chunks_read = reader.u64()?;
    let bytes_read = reader.u64()?;
    let resizes = reader.u64()?;
    let viewport_offset = reader.u32()? as usize;
    let max_viewport_offset = reader.u32()? as usize;
    if rows == 0
        || rows > MAX_SNAPSHOT_ROWS
        || cols == 0
        || cols > MAX_SNAPSHOT_COLS
        || viewport_offset > max_viewport_offset
    {
        return Err(());
    }
    let rows_data = decode_rows(reader, rows, cols)?;
    let snapshot = Snapshot {
        revision,
        input_sequence,
        rows,
        cols,
        cursor_row,
        cursor_col,
        cursor_visible: flags & 1 != 0,
        application_cursor: flags & 2 != 0,
        bracketed_paste: flags & 4 != 0,
        mouse_tracking: (flags >> 3) & 3,
        mouse_sgr: flags & 32 != 0,
        exited: flags & 64 != 0,
        exit_code: (raw_exit != i32::MIN).then_some(raw_exit),
        chunks_read,
        bytes_read,
        resizes,
        viewport_offset,
        max_viewport_offset,
        rows_data,
    };
    if layout_unit_count(&snapshot).is_none_or(|units| units > MAX_SNAPSHOT_LAYOUT_UNITS)
        || layout_key_count(&snapshot).is_none_or(|keys| keys > MAX_SNAPSHOT_LAYOUT_UNITS)
    {
        return Err(());
    }
    Ok(snapshot)
}

#[cfg(test)]
fn encode_view(view: &View) -> Result<Vec<u8>, ()> {
    encode_view_parts(view.snapshot.as_ref(), view.oversized)
}

fn encode_view_parts(snapshot: Option<&Snapshot>, oversized: bool) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(VIEW_MAGIC);
    bytes.push(FACET_CODEC_VERSION);
    bytes.push(if oversized {
        2
    } else if snapshot.is_some() {
        1
    } else {
        0
    });
    if let Some(snapshot) = snapshot.filter(|_| !oversized) {
        encode_snapshot(&mut bytes, snapshot)?;
    }
    (bytes.len() <= MAX_SNAPSHOT_ENCODED_BYTES)
        .then_some(bytes)
        .ok_or(())
}

fn decode_view(bytes: &[u8]) -> Result<View, ()> {
    if bytes.len() > MAX_SNAPSHOT_ENCODED_BYTES {
        return Err(());
    }
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != VIEW_MAGIC || reader.u8()? != FACET_CODEC_VERSION {
        return Err(());
    }
    let (snapshot, oversized) = match reader.u8()? {
        0 => (None, false),
        1 => (Some(decode_snapshot(&mut reader)?), false),
        2 => (None, true),
        _ => return Err(()),
    };
    if !reader.remaining().is_empty() {
        return Err(());
    }
    Ok(View {
        snapshot,
        oversized,
    })
}

fn encode_interaction(map: &Interaction) -> Result<Vec<u8>, ()> {
    let row_count = u16::try_from(map.rows_data.len()).map_err(|_| ())?;
    if row_count > map.rows || map.rows == 0 || map.cols == 0 {
        return Err(());
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(INTERACTION_MAGIC);
    bytes.push(FACET_CODEC_VERSION);
    push_u64(&mut bytes, map.revision);
    push_u16(&mut bytes, map.rows);
    push_u16(&mut bytes, map.cols);
    let modes = u8::from(map.application_cursor)
        | (u8::from(map.bracketed_paste) << 1)
        | ((map.mouse_tracking & 3) << 2)
        | (u8::from(map.mouse_sgr) << 4);
    bytes.push(modes);
    push_u32(
        &mut bytes,
        u32::try_from(map.viewport_offset).map_err(|_| ())?,
    );
    push_u32(
        &mut bytes,
        u32::try_from(map.max_viewport_offset).map_err(|_| ())?,
    );
    push_u16(&mut bytes, row_count);
    encode_rows(&mut bytes, &map.rows_data, map.cols)?;
    (bytes.len() <= MAX_SNAPSHOT_ENCODED_BYTES)
        .then_some(bytes)
        .ok_or(())
}

fn decode_interaction(bytes: &[u8]) -> Result<Interaction, ()> {
    if bytes.len() > MAX_SNAPSHOT_ENCODED_BYTES {
        return Err(());
    }
    let mut reader = Reader::new(bytes);
    if reader.take(4)? != INTERACTION_MAGIC || reader.u8()? != FACET_CODEC_VERSION {
        return Err(());
    }
    let revision = reader.u64()?;
    let rows = reader.u16()?;
    let cols = reader.u16()?;
    let modes = reader.u8()?;
    let viewport_offset = reader.u32()? as usize;
    let max_viewport_offset = reader.u32()? as usize;
    let text_rows = reader.u16()?;
    if rows == 0
        || rows > MAX_SNAPSHOT_ROWS
        || cols == 0
        || cols > MAX_SNAPSHOT_COLS
        || text_rows > rows
        || viewport_offset > max_viewport_offset
    {
        return Err(());
    }
    let rows_data = decode_rows(&mut reader, text_rows, cols)?;
    if !reader.remaining().is_empty() {
        return Err(());
    }
    Ok(Interaction {
        revision,
        rows,
        cols,
        cell_width: CELL_WIDTH,
        line_height: REQUESTED_LINE_HEIGHT,
        application_cursor: modes & 1 != 0,
        bracketed_paste: modes & 2 != 0,
        mouse_tracking: (modes >> 2) & 3,
        mouse_sgr: modes & 16 != 0,
        viewport_offset,
        max_viewport_offset,
        rows_data,
    })
}

#[derive(Clone, Debug, PartialEq)]
enum Intent {
    Key {
        logical_key: String,
        modifiers: u32,
        application_cursor: bool,
    },
    Text(String),
    Paste {
        text: String,
        bracketed: bool,
    },
    Mouse {
        revision: u64,
        tracking: u8,
        sgr: bool,
        point: CellPoint,
        button: u8,
        pressed: bool,
        modifiers: u32,
    },
    Viewport {
        revision: u64,
        offset: usize,
    },
    SetClipboard {
        revision: u64,
        text: String,
    },
    RequestClipboardPaste,
}

#[derive(Clone, Debug, PartialEq)]
enum ActorIntent {
    Key {
        logical_key: String,
        modifiers: u32,
        application_cursor: bool,
    },
    Text(String),
    PasteChunk {
        text: String,
        bracketed: bool,
        first: bool,
        last: bool,
    },
    Mouse {
        revision: u64,
        tracking: u8,
        sgr: bool,
        point: CellPoint,
        button: u8,
        pressed: bool,
        modifiers: u32,
    },
    Viewport {
        revision: u64,
        offset: usize,
    },
    ClipboardChunk {
        revision: u64,
        text: String,
        first: bool,
        last: bool,
    },
    RequestClipboardPaste,
}

fn push_string(bytes: &mut Vec<u8>, text: &str) -> Result<(), ()> {
    push_u32(bytes, u32::try_from(text.len()).map_err(|_| ())?);
    bytes.extend_from_slice(text.as_bytes());
    Ok(())
}

fn read_string(reader: &mut Reader<'_>) -> Result<String, ()> {
    let len = reader.u32()? as usize;
    std::str::from_utf8(reader.take(len)?)
        .map(str::to_owned)
        .map_err(|_| ())
}

fn encode_actor_intent(intent: &ActorIntent) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    match intent {
        ActorIntent::Key {
            logical_key,
            modifiers,
            application_cursor,
        } => {
            bytes.push(0);
            push_u32(&mut bytes, *modifiers);
            bytes.push(u8::from(*application_cursor));
            push_string(&mut bytes, logical_key)?;
        }
        ActorIntent::Text(text) => {
            bytes.push(1);
            push_string(&mut bytes, text)?;
        }
        ActorIntent::PasteChunk {
            text,
            bracketed,
            first,
            last,
        } => {
            bytes.push(2);
            bytes.push(u8::from(*bracketed) | (u8::from(*first) << 1) | (u8::from(*last) << 2));
            push_string(&mut bytes, text)?;
        }
        ActorIntent::Mouse {
            revision,
            tracking,
            sgr,
            point,
            button,
            pressed,
            modifiers,
        } => {
            bytes.push(3);
            push_u64(&mut bytes, *revision);
            bytes.push(*tracking);
            bytes.push(u8::from(*sgr));
            push_u16(&mut bytes, point.row);
            push_u16(&mut bytes, point.col);
            bytes.push(*button);
            bytes.push(u8::from(*pressed));
            push_u32(&mut bytes, *modifiers);
        }
        ActorIntent::Viewport { revision, offset } => {
            bytes.push(4);
            push_u64(&mut bytes, *revision);
            push_u32(&mut bytes, u32::try_from(*offset).map_err(|_| ())?);
        }
        ActorIntent::ClipboardChunk {
            revision,
            text,
            first,
            last,
        } => {
            bytes.push(5);
            push_u64(&mut bytes, *revision);
            bytes.push(u8::from(*first) | (u8::from(*last) << 1));
            push_string(&mut bytes, text)?;
        }
        ActorIntent::RequestClipboardPaste => bytes.push(6),
    }
    Ok(bytes)
}

fn decode_actor_intent(bytes: &[u8]) -> Result<ActorIntent, ()> {
    let mut reader = Reader::new(bytes);
    let intent = match reader.u8()? {
        0 => {
            let modifiers = reader.u32()?;
            let application_cursor = match reader.u8()? {
                0 => false,
                1 => true,
                _ => return Err(()),
            };
            ActorIntent::Key {
                logical_key: read_string(&mut reader)?,
                modifiers,
                application_cursor,
            }
        }
        1 => ActorIntent::Text(read_string(&mut reader)?),
        2 => {
            let flags = reader.u8()?;
            if flags & !0b111 != 0 {
                return Err(());
            }
            ActorIntent::PasteChunk {
                text: read_string(&mut reader)?,
                bracketed: flags & 1 != 0,
                first: flags & 2 != 0,
                last: flags & 4 != 0,
            }
        }
        3 => {
            let revision = reader.u64()?;
            let tracking = reader.u8()?;
            let sgr = match reader.u8()? {
                0 => false,
                1 => true,
                _ => return Err(()),
            };
            let point = CellPoint {
                row: reader.u16()?,
                col: reader.u16()?,
            };
            let button = reader.u8()?;
            let pressed = match reader.u8()? {
                0 => false,
                1 => true,
                _ => return Err(()),
            };
            ActorIntent::Mouse {
                revision,
                tracking,
                sgr,
                point,
                button,
                pressed,
                modifiers: reader.u32()?,
            }
        }
        4 => ActorIntent::Viewport {
            revision: reader.u64()?,
            offset: reader.u32()? as usize,
        },
        5 => {
            let revision = reader.u64()?;
            let flags = reader.u8()?;
            if flags & !0b11 != 0 {
                return Err(());
            }
            ActorIntent::ClipboardChunk {
                revision,
                text: read_string(&mut reader)?,
                first: flags & 1 != 0,
                last: flags & 2 != 0,
            }
        }
        6 => ActorIntent::RequestClipboardPaste,
        _ => return Err(()),
    };
    if !reader.remaining().is_empty() {
        return Err(());
    }
    Ok(intent)
}

fn text_chunks(text: &str, max_bytes: usize) -> Result<Vec<&str>, ()> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if max_bytes == 0 {
        return Err(());
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = start.saturating_add(max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            return Err(());
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    Ok(chunks)
}

fn encode_actor_events(
    intents: Vec<Intent>,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<Vec<Vec<u8>>, ()> {
    // Actor events subsequently cross the shared-memory mailbox. Keep each
    // compact record well below that transport's envelope while preserving the
    // full input as one ordered, atomic facet batch.
    const MAX_ACTOR_EVENT_BYTES: usize = 32 * 1024;
    let max_event_bytes = max_event_bytes.min(MAX_ACTOR_EVENT_BYTES);
    let mut events = Vec::new();
    let mut push = |intent: ActorIntent| -> Result<(), ()> {
        if events.len() >= max_events {
            return Err(());
        }
        let bytes = encode_actor_intent(&intent)?;
        if bytes.len() > max_event_bytes {
            return Err(());
        }
        events.push(bytes);
        Ok(())
    };
    for intent in intents {
        match intent {
            Intent::Key {
                logical_key,
                modifiers,
                application_cursor,
            } => push(ActorIntent::Key {
                logical_key,
                modifiers,
                application_cursor,
            })?,
            Intent::Text(text) => {
                let chunk_bytes = max_event_bytes.checked_sub(5).ok_or(())?;
                for chunk in text_chunks(&text, chunk_bytes)? {
                    push(ActorIntent::Text(chunk.to_owned()))?;
                }
            }
            Intent::Paste { text, bracketed } => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let chunk_bytes = max_event_bytes.checked_sub(6).ok_or(())?;
                let chunks = text_chunks(&normalized, chunk_bytes)?;
                let last = chunks.len().saturating_sub(1);
                for (index, chunk) in chunks.into_iter().enumerate() {
                    push(ActorIntent::PasteChunk {
                        text: chunk.to_owned(),
                        bracketed,
                        first: index == 0,
                        last: index == last,
                    })?;
                }
            }
            Intent::Mouse {
                revision,
                tracking,
                sgr,
                point,
                button,
                pressed,
                modifiers,
            } => push(ActorIntent::Mouse {
                revision,
                tracking,
                sgr,
                point,
                button,
                pressed,
                modifiers,
            })?,
            Intent::Viewport { revision, offset } => {
                push(ActorIntent::Viewport { revision, offset })?;
            }
            Intent::SetClipboard { revision, text } => {
                let chunk_bytes = max_event_bytes.checked_sub(14).ok_or(())?;
                let chunks = text_chunks(&text, chunk_bytes)?;
                let last = chunks.len().saturating_sub(1);
                for (index, chunk) in chunks.into_iter().enumerate() {
                    push(ActorIntent::ClipboardChunk {
                        revision,
                        text: chunk.to_owned(),
                        first: index == 0,
                        last: index == last,
                    })?;
                }
            }
            Intent::RequestClipboardPaste => push(ActorIntent::RequestClipboardPaste)?,
        }
    }
    Ok(events)
}

static PUBLICATION: Mutex<(u64, Vec<u8>)> = Mutex::new((0, Vec::new()));

pub(super) fn snapshot_fits(snapshot: &Snapshot) -> bool {
    encode_view_parts(Some(snapshot), false).is_ok()
}

pub(super) fn publish() {
    let Ok(guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_ref().filter(|state| state.pane != 0) else {
        return;
    };
    let bytes = encode_view_parts(state.latest.as_deref(), false)
        .unwrap_or_else(|()| encode_view_parts(None, true).unwrap_or_else(|()| Vec::new()));
    drop(guard);
    if bytes.is_empty() {
        return;
    }
    let mut published = PUBLICATION.lock().expect("terminal publication lock");
    if published.1 == bytes {
        return;
    }
    let revision = published
        .0
        .checked_add(1)
        .expect("terminal publication revision exhausted");
    if brood_plugin_sdk::publication::publish(revision, &bytes).is_err() {
        return;
    }
    *published = (revision, bytes);
}

pub(super) fn handle_input(events: Vec<Vec<u8>>) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    let mut paste = None::<(bool, Vec<String>, usize)>;
    let mut clipboard = None::<(u64, String)>;
    for bytes in events {
        let Ok(intent) = decode_actor_intent(&bytes) else {
            continue;
        };
        match intent {
            ActorIntent::Key {
                logical_key,
                modifiers,
                application_cursor,
            } if state
                .latest
                .as_ref()
                .is_some_and(|snapshot| snapshot.application_cursor == application_cursor) =>
            {
                if let Some(bytes) = key_bytes(&logical_key, modifiers, application_cursor) {
                    state.requested_viewport = 0;
                    commands.extend(state.input_commands(&bytes));
                }
            }
            ActorIntent::Text(text) => {
                let printable = text
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect::<String>();
                if !printable.is_empty() {
                    state.requested_viewport = 0;
                    commands.extend(state.input_commands(printable.as_bytes()));
                }
            }
            ActorIntent::PasteChunk {
                text,
                bracketed,
                first,
                last,
            } => {
                let mode_matches = state
                    .latest
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.bracketed_paste == bracketed);
                if !mode_matches {
                    paste = None;
                    continue;
                }
                if first {
                    paste = Some((bracketed, Vec::new(), 0));
                }
                let Some((pending_mode, pending, pending_bytes)) = paste.as_mut() else {
                    continue;
                };
                if *pending_mode != bracketed
                    || pending_bytes
                        .checked_add(text.len())
                        .is_none_or(|len| len > MAX_SNAPSHOT_ENCODED_BYTES)
                {
                    paste = None;
                    continue;
                }
                *pending_bytes += text.len();
                pending.push(text);
                if last {
                    let Some((bracketed, chunks, _)) = paste.take() else {
                        continue;
                    };
                    state.requested_viewport = 0;
                    let payload_bytes = chunks.iter().map(String::len).sum::<usize>();
                    let mut bytes = Vec::with_capacity(payload_bytes + usize::from(bracketed) * 12);
                    if bracketed {
                        bytes.extend_from_slice(b"\x1b[200~");
                    }
                    for chunk in chunks {
                        bytes.extend_from_slice(chunk.as_bytes());
                    }
                    if bracketed {
                        bytes.extend_from_slice(b"\x1b[201~");
                    }
                    commands.extend(state.input_commands(&bytes));
                }
            }
            ActorIntent::Mouse {
                revision,
                tracking,
                sgr,
                point,
                button,
                pressed,
                modifiers,
            } => {
                let valid = state.latest.as_ref().is_some_and(|snapshot| {
                    revision == snapshot.revision
                        && tracking > 0
                        && snapshot.mouse_tracking == tracking
                        && snapshot.mouse_sgr == sgr
                        && point.row < snapshot.rows
                        && point.col < snapshot.cols
                });
                if valid && let Some(bytes) = mouse_report(point, button, pressed, modifiers, sgr) {
                    commands.extend(state.input_commands(&bytes));
                }
            }
            ActorIntent::Viewport { revision, offset }
                if state.latest.as_ref().is_some_and(|snapshot| {
                    revision == snapshot.revision && offset <= snapshot.max_viewport_offset
                }) =>
            {
                state.requested_viewport = offset;
                commands.push(state.viewport_command(offset));
            }
            ActorIntent::ClipboardChunk {
                revision,
                text,
                first,
                last,
            } => {
                let revision_matches = state
                    .latest
                    .as_ref()
                    .is_some_and(|snapshot| revision == snapshot.revision);
                if !revision_matches {
                    clipboard = None;
                    continue;
                }
                if first {
                    clipboard = Some((revision, String::new()));
                }
                let Some((pending_revision, pending)) = clipboard.as_mut() else {
                    continue;
                };
                if *pending_revision != revision
                    || pending
                        .len()
                        .checked_add(text.len())
                        .is_none_or(|len| len > MAX_SNAPSHOT_ENCODED_BYTES)
                {
                    clipboard = None;
                    continue;
                }
                pending.push_str(&text);
                if last {
                    let Some((_, text)) = clipboard.take() else {
                        continue;
                    };
                    if !text.is_empty() {
                        commands.extend(set_clipboard_command(text));
                    }
                }
            }
            ActorIntent::RequestClipboardPaste if state.pane != 0 => {
                commands.push(request_clipboard_paste_command(state.pane));
            }
            _ => {}
        }
    }
    commands
}

fn decode_limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 262_144,
    }
}

fn pointer_cell(map: &Interaction, x: f64, y: f64) -> CellPoint {
    let row = (((y as f32) - PADDING) / map.line_height)
        .floor()
        .clamp(0.0, f32::from(map.rows.saturating_sub(1))) as u16;
    let col = (((x as f32) - PADDING) / map.cell_width)
        .floor()
        .clamp(0.0, f32::from(map.cols.saturating_sub(1))) as u16;
    CellPoint { row, col }
}

fn selection_text_from_map(map: &Interaction, selection: Selection) -> Option<String> {
    let (start, end) = normalized_selection(selection);
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let runs = map.rows_data.get(usize::from(row))?;
        let cells = row_cells(map.cols, runs);
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row {
            end.col
        } else {
            map.cols.saturating_sub(1)
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

fn key_intents(
    map: &Interaction,
    sequence: &mut Sequence,
    logical_key: String,
    modifiers: u32,
) -> Vec<Intent> {
    sequence.suppressed_text = ((modifiers & (MOD_CTRL | MOD_ALT)) != 0
        && logical_key.chars().count() == 1)
        .then(|| logical_key.clone());
    let copy = modifiers & (MOD_CTRL | MOD_SHIFT) == (MOD_CTRL | MOD_SHIFT)
        && logical_key.eq_ignore_ascii_case("c");
    if copy {
        return sequence
            .selection
            .and_then(|selection| selection_text_from_map(map, selection))
            .map(|text| Intent::SetClipboard {
                revision: map.revision,
                text,
            })
            .into_iter()
            .collect();
    }
    let paste = (modifiers & (MOD_CTRL | MOD_SHIFT) == (MOD_CTRL | MOD_SHIFT)
        && logical_key.eq_ignore_ascii_case("v"))
        || (modifiers & MOD_SHIFT != 0 && logical_key == "Insert");
    if paste {
        return vec![Intent::RequestClipboardPaste];
    }
    let page = usize::from(map.rows.saturating_sub(1)).max(1);
    let viewport = match (modifiers & MOD_SHIFT != 0, logical_key.as_str()) {
        (true, "PageUp") => Some(
            map.viewport_offset
                .saturating_add(page)
                .min(map.max_viewport_offset),
        ),
        (true, "PageDown") => Some(map.viewport_offset.saturating_sub(page)),
        (true, "Home") => Some(map.max_viewport_offset),
        (true, "End") => Some(0),
        _ => None,
    };
    if let Some(offset) = viewport {
        return vec![Intent::Viewport {
            revision: map.revision,
            offset,
        }];
    }
    key_bytes(&logical_key, modifiers, map.application_cursor)
        .map(|_| Intent::Key {
            logical_key,
            modifiers,
            application_cursor: map.application_cursor,
        })
        .into_iter()
        .collect()
}

fn cancel_sequence(sequence: &mut Sequence) -> Vec<facets::HostRequest> {
    let captured = sequence
        .selection
        .is_some_and(|selection| selection.dragging)
        || sequence.pressed_mouse_button.is_some();
    sequence.selection = sequence.selection.map(|mut selection| {
        selection.dragging = false;
        selection
    });
    sequence.pressed_mouse_button = None;
    sequence.last_pointer = None;
    sequence.mouse_modifiers = 0;
    sequence.suppressed_text = None;
    captured
        .then_some(facets::HostRequest::ReleaseCapture)
        .into_iter()
        .collect()
}

fn reduce(
    map: &Interaction,
    sequence: &mut Sequence,
    event: facets::InputEvent,
) -> (Vec<Intent>, Vec<facets::HostRequest>) {
    let mut intents = Vec::new();
    let mut requests = Vec::new();
    match event {
        facets::InputEvent::Key {
            logical_key,
            modifiers,
            pressed: true,
            ..
        } => intents.extend(key_intents(map, sequence, logical_key, modifiers)),
        facets::InputEvent::Text { text }
        | facets::InputEvent::Composition {
            text,
            committed: true,
        } if !text.is_empty() => {
            if !sequence
                .suppressed_text
                .take()
                .is_some_and(|key| key.eq_ignore_ascii_case(&text))
            {
                let printable = text
                    .chars()
                    .filter(|character| !character.is_control())
                    .collect::<String>();
                if !printable.is_empty() {
                    intents.push(Intent::Text(printable));
                }
            }
        }
        facets::InputEvent::Paste { text } if !text.is_empty() => {
            intents.push(Intent::Paste {
                text,
                bracketed: map.bracketed_paste,
            });
        }
        facets::InputEvent::PointerMotion { x, y } if x.is_finite() && y.is_finite() => {
            let point = pointer_cell(map, x, y);
            sequence.last_pointer = Some(point);
            if sequence
                .selection
                .is_some_and(|selection| selection.dragging)
            {
                if let Some(selection) = sequence.selection.as_mut()
                    && selection.extent != point
                {
                    selection.extent = point;
                    requests.push(facets::HostRequest::Render);
                }
            } else if map.mouse_tracking >= 2
                && (map.mouse_tracking != 2 || sequence.pressed_mouse_button.is_some())
            {
                intents.push(Intent::Mouse {
                    revision: map.revision,
                    tracking: map.mouse_tracking,
                    sgr: map.mouse_sgr,
                    point,
                    button: sequence
                        .pressed_mouse_button
                        .unwrap_or(3)
                        .saturating_add(32),
                    pressed: true,
                    modifiers: sequence.mouse_modifiers,
                });
            }
        }
        facets::InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
            modifiers,
        } if x.is_finite() && y.is_finite() => {
            let point = pointer_cell(map, x, y);
            sequence.last_pointer = Some(point);
            let selecting = sequence
                .selection
                .is_some_and(|selection| selection.dragging);
            let local = selecting
                || (sequence.pressed_mouse_button.is_none()
                    && (map.mouse_tracking == 0 || modifiers & MOD_SHIFT != 0));
            if local && button == MIDDLE_BUTTON && pressed {
                intents.push(Intent::RequestClipboardPaste);
                requests.push(facets::HostRequest::Focus);
            } else if local && button == LEFT_BUTTON {
                if pressed {
                    sequence.selection = Some(Selection {
                        anchor: point,
                        extent: point,
                        dragging: true,
                    });
                    requests.extend([
                        facets::HostRequest::Focus,
                        facets::HostRequest::Capture,
                        facets::HostRequest::Render,
                    ]);
                } else if let Some(selection) = sequence.selection.as_mut() {
                    selection.extent = point;
                    selection.dragging = false;
                    let selection = *selection;
                    if let Some(text) = selection_text_from_map(map, selection) {
                        intents.push(Intent::SetClipboard {
                            revision: map.revision,
                            text,
                        });
                    }
                    requests.extend([
                        facets::HostRequest::ReleaseCapture,
                        facets::HostRequest::Render,
                    ]);
                }
            } else if !local && let Some(mouse_button) = mouse_button_number(button) {
                let reported = if pressed {
                    sequence.pressed_mouse_button = Some(mouse_button);
                    sequence.mouse_modifiers = modifiers;
                    requests.extend([facets::HostRequest::Focus, facets::HostRequest::Capture]);
                    mouse_button
                } else {
                    sequence.pressed_mouse_button = None;
                    sequence.mouse_modifiers = 0;
                    requests.push(facets::HostRequest::ReleaseCapture);
                    if map.mouse_sgr { mouse_button } else { 3 }
                };
                intents.push(Intent::Mouse {
                    revision: map.revision,
                    tracking: map.mouse_tracking,
                    sgr: map.mouse_sgr,
                    point,
                    button: reported,
                    pressed,
                    modifiers,
                });
            }
        }
        facets::InputEvent::PointerScroll {
            x,
            y,
            vertical,
            modifiers,
            ..
        } if x.is_finite() && y.is_finite() && vertical.is_finite() => {
            sequence.last_pointer = Some(pointer_cell(map, x, y));
            if map.mouse_tracking > 0 && modifiers & MOD_SHIFT == 0 {
                sequence.mouse_wheel_accumulator += vertical as f32 / MOUSE_WHEEL_PIXELS_PER_CLICK;
                let clicks = sequence.mouse_wheel_accumulator.trunc() as isize;
                sequence.mouse_wheel_accumulator -= clicks as f32;
                let button = if clicks > 0 { 64 } else { 65 };
                intents.extend((0..clicks.unsigned_abs().min(64)).map(|_| Intent::Mouse {
                    revision: map.revision,
                    tracking: map.mouse_tracking,
                    sgr: map.mouse_sgr,
                    point: pointer_cell(map, x, y),
                    button,
                    pressed: true,
                    modifiers,
                }));
            } else {
                sequence.scroll_accumulator += vertical as f32 / SCROLL_PIXELS_PER_LINE;
                let delta = sequence.scroll_accumulator.trunc() as isize;
                sequence.scroll_accumulator -= delta as f32;
                let next = map
                    .viewport_offset
                    .saturating_add_signed(delta)
                    .min(map.max_viewport_offset);
                if delta != 0 && next != map.viewport_offset {
                    intents.push(Intent::Viewport {
                        revision: map.revision,
                        offset: next,
                    });
                }
            }
        }
        facets::InputEvent::FocusLost
        | facets::InputEvent::CaptureLost
        | facets::InputEvent::PointerLeave => {
            if let (Some(button), Some(point)) =
                (sequence.pressed_mouse_button, sequence.last_pointer)
            {
                intents.push(Intent::Mouse {
                    revision: map.revision,
                    tracking: map.mouse_tracking,
                    sgr: map.mouse_sgr,
                    point,
                    button: if map.mouse_sgr { button } else { 3 },
                    pressed: false,
                    modifiers: sequence.mouse_modifiers,
                });
            }
            requests.extend(cancel_sequence(sequence));
            requests.push(facets::HostRequest::Render);
        }
        _ => {}
    }
    (intents, requests)
}

fn cursor_phase(target_time_ns: u64, focused: bool) -> (bool, Option<u64>) {
    if !focused {
        return (true, None);
    }
    let phase = target_time_ns / CURSOR_BLINK_NS;
    let visible = phase.is_multiple_of(2);
    let next = phase
        .checked_add(1)
        .and_then(|phase| phase.checked_mul(CURSOR_BLINK_NS));
    (visible, next)
}

fn layout_terminal_text(
    cache: &mut HashMap<RunKey, u64>,
    pane: u64,
    run: &Run,
    text: &str,
    cells: u16,
    cell_width: f32,
    line_height: f32,
) -> u64 {
    let key = RunKey {
        cells,
        fg: run.fg,
        bold: run.bold,
        faint: run.faint,
        italic: run.italic,
        text: text.to_owned(),
    };
    if let Some(handle) = cache.get(&key) {
        return *handle;
    }
    let color = rgb_channels(run.fg);
    let handle = host::graphics_layout_text(
        pane,
        text,
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
    .expect("lease terminal text layout")
    .handle;
    cache.insert(key, handle);
    handle
}

fn pane_packet(
    pane: &facets::PaneTarget,
    snapshot: &Snapshot,
    sequence: &Sequence,
    target_time_ns: u64,
) -> (Vec<u8>, Interaction, Option<u64>) {
    let width = pane.logical_width as f32;
    let height = pane.logical_height as f32;
    let cell_width = CELL_WIDTH;
    let line_height = REQUESTED_LINE_HEIGHT;
    let selection = sequence.selection;
    let (blink_visible, next_frame_ns) = cursor_phase(target_time_ns, pane.focused);
    let mut text_layouts = HashMap::new();
    let mut packet = Packet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, width, height),
        color: rgb(DEFAULT_BG),
    });
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
        if let Some((start, end)) =
            selection.and_then(|selection| selection_columns(selection, row as u16, snapshot.cols))
            && start < end
        {
            packet.push(DrawCommand::FillRect {
                rect: Rect::new(
                    PADDING + f32::from(start) * cell_width,
                    y,
                    f32::from(end - start) * cell_width,
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
                for (offset, cells, cluster) in terminal_text_clusters(text) {
                    if cluster != " " {
                        packet.push(DrawCommand::DrawTextLayout {
                            layout: layout_terminal_text(
                                &mut text_layouts,
                                pane.pane,
                                run,
                                &cluster,
                                cells,
                                cell_width,
                                line_height,
                            ),
                            x: x + f32::from(offset) * cell_width,
                            y,
                        });
                    }
                }
            } else {
                let cells = text
                    .chars()
                    .map(|character| UnicodeWidthChar::width(character).unwrap_or(1))
                    .sum::<usize>()
                    .max(1)
                    .min(usize::from(run.cells)) as u16;
                packet.push(DrawCommand::DrawTextLayout {
                    layout: layout_terminal_text(
                        &mut text_layouts,
                        pane.pane,
                        run,
                        text,
                        cells,
                        cell_width,
                        line_height,
                    ),
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
        && blink_visible
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
    let interaction = Interaction {
        revision: snapshot.revision,
        rows: snapshot.rows,
        cols: snapshot.cols,
        cell_width,
        line_height,
        application_cursor: snapshot.application_cursor,
        bracketed_paste: snapshot.bracketed_paste,
        mouse_tracking: snapshot.mouse_tracking,
        mouse_sgr: snapshot.mouse_sgr,
        viewport_offset: snapshot.viewport_offset,
        max_viewport_offset: snapshot.max_viewport_offset,
        rows_data: snapshot.rows_data.clone(),
    };
    (
        packet.encode().expect("encode terminal facet packet"),
        interaction,
        snapshot.cursor_visible.then_some(next_frame_ns).flatten(),
    )
}

fn placeholder_content(pane: &facets::PaneTarget, error: bool) -> facets::PaneContent {
    let width = pane.logical_width as f32;
    let height = pane.logical_height as f32;
    let mut packet = Packet::new();
    packet.push(DrawCommand::FillRect {
        rect: Rect::new(0.0, 0.0, width, height),
        color: rgb(DEFAULT_BG),
    });
    if error {
        packet.push(DrawCommand::FillRect {
            rect: Rect::new(0.0, height - 3.0, width, 3.0),
            color: rgb(ashen::ERROR_RGB),
        });
    }
    let rows = ((height - PADDING * 2.0) / REQUESTED_LINE_HEIGHT)
        .floor()
        .clamp(1.0, f32::from(MAX_SNAPSHOT_ROWS)) as u16;
    let cols = ((width - PADDING * 2.0) / CELL_WIDTH)
        .floor()
        .clamp(1.0, f32::from(MAX_SNAPSHOT_COLS)) as u16;
    let interaction = encode_interaction(&Interaction {
        revision: 0,
        rows,
        cols,
        cell_width: CELL_WIDTH,
        line_height: REQUESTED_LINE_HEIGHT,
        application_cursor: false,
        bracketed_paste: false,
        mouse_tracking: 0,
        mouse_sgr: false,
        viewport_offset: 0,
        max_viewport_offset: 0,
        rows_data: Vec::new(),
    })
    .unwrap_or_default();
    facets::PaneContent::Draw {
        packet: packet.encode().unwrap_or_default(),
        interaction,
    }
}

impl Facets for TerminalUi {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let Ok(request) = facet_codec::decode_render_request(&input, decode_limits(input.len()))
        else {
            return Vec::new();
        };
        let view = decode_view(&request.application_view).unwrap_or(View {
            snapshot: None,
            oversized: true,
        });
        let sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let mut next_frame_ns: Option<u64> = None;
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let content = if view.oversized {
                    placeholder_content(pane, true)
                } else {
                    view.snapshot.as_ref().map_or_else(
                        || placeholder_content(pane, false),
                        |snapshot| {
                            let (packet, interaction, next) =
                                pane_packet(pane, snapshot, &sequence, request.target_time_ns);
                            next_frame_ns = match (next_frame_ns, next) {
                                (Some(current), Some(next)) => Some(current.min(next)),
                                (None, next) => next,
                                (current, None) => current,
                            };
                            let interaction = encode_interaction(&interaction);
                            if packet.len() > request.limits.max_packet_bytes as usize
                                || match interaction.as_ref() {
                                    Ok(bytes) => {
                                        bytes.len() > request.limits.max_interaction_bytes as usize
                                    }
                                    Err(()) => true,
                                }
                            {
                                placeholder_content(pane, true)
                            } else {
                                facets::PaneContent::Draw {
                                    packet,
                                    interaction: interaction.unwrap_or_default(),
                                }
                            }
                        },
                    )
                };
                facets::PaneOutput {
                    pane: pane.pane,
                    content,
                }
            })
            .collect();
        let response = facets::RenderResponse {
            token: request.token,
            panes,
            next_frame_ns,
        };
        facet_codec::encode_render_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .unwrap_or_default()
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let Ok(request) = facet_codec::decode_input_request(&input, decode_limits(input.len()))
        else {
            return Vec::new();
        };
        let mut sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let (events, requests) = match decode_interaction(&request.interaction) {
            Ok(map) => reduce(&map, &mut sequence, request.event),
            Err(()) => {
                sequence = Sequence::default();
                (Vec::new(), vec![facets::HostRequest::ReleaseCapture])
            }
        };
        let events = encode_actor_events(
            events,
            request.limits.max_event_bytes as usize,
            request.limits.max_events as usize,
        )
        .unwrap_or_default();
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: serde_json::to_vec(&sequence).expect("terminal input sequence"),
            events,
            requests,
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(tracking: u8) -> Interaction {
        Interaction {
            revision: 7,
            rows: 2,
            cols: 4,
            cell_width: CELL_WIDTH,
            line_height: REQUESTED_LINE_HEIGHT,
            application_cursor: false,
            bracketed_paste: true,
            mouse_tracking: tracking,
            mouse_sgr: true,
            viewport_offset: 1,
            max_viewport_offset: 5,
            rows_data: vec![
                vec![Run {
                    start: 0,
                    cells: 4,
                    fg: DEFAULT_BG,
                    bg: DEFAULT_BG,
                    bold: false,
                    faint: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    text: "a界 ".into(),
                }],
                vec![Run {
                    start: 0,
                    cells: 4,
                    fg: DEFAULT_BG,
                    bg: DEFAULT_BG,
                    bold: false,
                    faint: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    text: "bc  ".into(),
                }],
            ],
        }
    }

    fn button(button: u32, pressed: bool, modifiers: u32) -> facets::InputEvent {
        facets::InputEvent::PointerButton {
            x: 19.0,
            y: 9.0,
            button,
            pressed,
            modifiers,
        }
    }

    #[test]
    fn selection_is_frame_pinned_and_capture_has_an_explicit_lifetime() {
        let map = map(0);
        let mut sequence = Sequence::default();
        let (_, requests) = reduce(&map, &mut sequence, button(LEFT_BUTTON, true, 0));
        assert_eq!(
            requests,
            vec![
                facets::HostRequest::Focus,
                facets::HostRequest::Capture,
                facets::HostRequest::Render
            ]
        );
        reduce(
            &map,
            &mut sequence,
            facets::InputEvent::PointerMotion { x: 31.0, y: 29.0 },
        );
        let (intents, requests) = reduce(
            &map,
            &mut sequence,
            facets::InputEvent::PointerButton {
                x: 31.0,
                y: 29.0,
                button: LEFT_BUTTON,
                pressed: false,
                modifiers: 0,
            },
        );
        assert_eq!(
            requests,
            vec![
                facets::HostRequest::ReleaseCapture,
                facets::HostRequest::Render
            ]
        );
        assert_eq!(
            intents,
            vec![Intent::SetClipboard {
                revision: 7,
                text: "界\nbc".into()
            }]
        );
    }

    #[test]
    fn mouse_tracking_and_loss_preserve_press_motion_release_order() {
        let map = map(3);
        let mut sequence = Sequence::default();
        let (press, requests) = reduce(&map, &mut sequence, button(LEFT_BUTTON, true, 0));
        assert!(requests.contains(&facets::HostRequest::Capture));
        assert!(matches!(
            press.as_slice(),
            [Intent::Mouse {
                button: 0,
                pressed: true,
                ..
            }]
        ));
        let (motion, _) = reduce(
            &map,
            &mut sequence,
            facets::InputEvent::PointerMotion { x: 21.0, y: 21.0 },
        );
        assert!(matches!(
            motion.as_slice(),
            [Intent::Mouse { button: 32, .. }]
        ));
        let (_, lost) = reduce(&map, &mut sequence, facets::InputEvent::CaptureLost);
        assert_eq!(
            lost,
            vec![
                facets::HostRequest::ReleaseCapture,
                facets::HostRequest::Render
            ]
        );
        assert!(sequence.pressed_mouse_button.is_none());
    }

    #[test]
    fn keyboard_text_paste_and_viewport_intents_keep_terminal_modes() {
        let map = map(0);
        let mut sequence = Sequence::default();
        let (key, _) = reduce(
            &map,
            &mut sequence,
            facets::InputEvent::Key {
                code: 0,
                logical_key: "ArrowUp".into(),
                modifiers: 0,
                pressed: true,
                repeat: false,
            },
        );
        assert_eq!(
            key,
            vec![Intent::Key {
                logical_key: "ArrowUp".into(),
                modifiers: 0,
                application_cursor: false
            }]
        );
        assert_eq!(
            reduce(
                &map,
                &mut sequence,
                facets::InputEvent::Paste { text: "x".into() }
            )
            .0,
            vec![Intent::Paste {
                text: "x".into(),
                bracketed: true
            }]
        );
        let page_up = facets::InputEvent::Key {
            code: 0,
            logical_key: "PageUp".into(),
            modifiers: MOD_SHIFT,
            pressed: true,
            repeat: false,
        };
        assert_eq!(
            reduce(&map, &mut sequence, page_up).0,
            vec![Intent::Viewport {
                revision: 7,
                offset: 2
            }]
        );
    }

    #[test]
    fn cursor_blink_is_derived_only_from_host_time_and_focus() {
        assert_eq!(cursor_phase(0, true), (true, Some(CURSOR_BLINK_NS)));
        assert_eq!(
            cursor_phase(CURSOR_BLINK_NS, true),
            (false, Some(CURSOR_BLINK_NS * 2))
        );
        assert_eq!(
            cursor_phase(CURSOR_BLINK_NS * 2, true),
            (true, Some(CURSOR_BLINK_NS * 3))
        );
        assert_eq!(cursor_phase(CURSOR_BLINK_NS, false), (true, None));
    }

    #[test]
    fn actor_grid_and_frame_map_share_cell_boundaries() {
        let mut actor = UiState::new(2, 7, "terminal-ui-b2-s7");
        actor.width = PADDING * 2.0 + CELL_WIDTH * 4.0;
        actor.height = PADDING * 2.0 + REQUESTED_LINE_HEIGHT * 2.0;
        assert_eq!(actor.grid_size(), (2, 4));

        let map = map(0);
        assert_eq!(
            pointer_cell(
                &map,
                f64::from(PADDING + CELL_WIDTH * 3.0),
                f64::from(PADDING + REQUESTED_LINE_HEIGHT)
            ),
            CellPoint { row: 1, col: 3 }
        );
        assert_eq!(
            pointer_cell(&map, f64::from(actor.width), f64::from(actor.height)),
            CellPoint { row: 1, col: 3 }
        );
    }

    #[test]
    fn sparse_wide_interaction_remains_compact() {
        let mut map = map(0);
        map.rows = 1;
        map.cols = 4_096;
        map.rows_data = vec![vec![Run {
            start: 4_095,
            cells: 1,
            fg: DEFAULT_BG,
            bg: DEFAULT_BG,
            bold: false,
            faint: false,
            italic: false,
            underline: false,
            strikethrough: false,
            text: "x".into(),
        }]];
        let bytes = encode_interaction(&map).expect("compact interaction");
        assert!(
            bytes.len() < 1_024,
            "wide sparse map used {} bytes",
            bytes.len()
        );
        assert_eq!(decode_interaction(&bytes), Ok(map.clone()));
        assert_eq!(
            selection_text_from_map(
                &map,
                Selection {
                    anchor: CellPoint { row: 0, col: 4_095 },
                    extent: CellPoint { row: 0, col: 4_095 },
                    dragging: false,
                }
            ),
            Some("x".into())
        );
    }

    #[test]
    fn binary_interaction_keeps_high_escape_text_raw() {
        let mut map = map(0);
        let text = "\"\\\n\r\t\u{0008}界";
        map.rows_data[0][0].text = text.into();
        let bytes = encode_interaction(&map).expect("binary interaction");
        assert!(
            bytes
                .windows(text.len())
                .any(|window| window == text.as_bytes())
        );
        assert_eq!(decode_interaction(&bytes), Ok(map));
    }

    #[test]
    fn view_codec_roundtrips_with_exact_compact_size() {
        let map = map(0);
        let snapshot = Snapshot {
            revision: map.revision,
            input_sequence: 6,
            rows: map.rows,
            cols: map.cols,
            cursor_row: 1,
            cursor_col: 2,
            cursor_visible: true,
            application_cursor: map.application_cursor,
            bracketed_paste: map.bracketed_paste,
            mouse_tracking: map.mouse_tracking,
            mouse_sgr: map.mouse_sgr,
            exited: false,
            exit_code: None,
            chunks_read: 3,
            bytes_read: 99,
            resizes: 2,
            viewport_offset: map.viewport_offset,
            max_viewport_offset: map.max_viewport_offset,
            rows_data: map.rows_data,
        };
        let text_bytes = snapshot
            .rows_data
            .iter()
            .flatten()
            .map(|run| run.text.len())
            .sum::<usize>();
        let runs = snapshot.rows_data.iter().map(Vec::len).sum::<usize>();
        let expected = 67 + usize::from(snapshot.rows) * 2 + runs * 15 + text_bytes;
        let view = View {
            snapshot: Some(snapshot),
            oversized: false,
        };
        let bytes = encode_view(&view).expect("compact view");
        assert_eq!(bytes.len(), expected);
        assert_eq!(decode_view(&bytes), Ok(view));
    }

    #[test]
    fn eighty_by_twenty_four_individually_styled_cells_fit_contract() {
        let rows_data = (0_u16..24)
            .map(|row| {
                (0_u16..80)
                    .map(|col| Run {
                        start: col,
                        cells: 1,
                        fg: u32::from(row) * 80 + u32::from(col),
                        bg: DEFAULT_BG,
                        bold: false,
                        faint: false,
                        italic: false,
                        underline: false,
                        strikethrough: false,
                        text: "x".into(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let snapshot = Snapshot {
            revision: 1,
            input_sequence: 0,
            rows: 24,
            cols: 80,
            cursor_row: 23,
            cursor_col: 79,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: false,
            mouse_tracking: 0,
            mouse_sgr: false,
            exited: false,
            exit_code: None,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            viewport_offset: 0,
            max_viewport_offset: 0,
            rows_data,
        };
        assert_eq!(layout_unit_count(&snapshot), Some(1_920));
        assert_eq!(layout_key_count(&snapshot), Some(1_920));
        assert!(snapshot_fits(&snapshot));
        let bytes = encode_view_parts(Some(&snapshot), false).expect("dense terminal view");
        assert!(bytes.len() < MAX_SNAPSHOT_ENCODED_BYTES);
    }

    #[test]
    fn unicode_cursor_row_keeps_wide_and_combining_layout_units() {
        let mut snapshot = Snapshot {
            revision: 1,
            input_sequence: 0,
            rows: 1,
            cols: 6,
            cursor_row: 0,
            cursor_col: 3,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: false,
            mouse_tracking: 0,
            mouse_sgr: false,
            exited: false,
            exit_code: None,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            viewport_offset: 0,
            max_viewport_offset: 0,
            rows_data: vec![vec![Run {
                start: 0,
                cells: 6,
                fg: DEFAULT_BG,
                bg: DEFAULT_BG,
                bold: false,
                faint: false,
                italic: false,
                underline: false,
                strikethrough: false,
                text: "a界e\u{301}  ".into(),
            }]],
        };
        assert_eq!(layout_unit_count(&snapshot), Some(3));
        assert_eq!(layout_key_count(&snapshot), Some(3));
        snapshot.rows_data[0][0].text = "界界".into();
        assert_eq!(layout_unit_count(&snapshot), Some(2));
        assert_eq!(layout_key_count(&snapshot), Some(1));
    }

    #[test]
    fn host_limit_mismatch_uses_bounded_visible_fallback() {
        let pane = facets::PaneTarget {
            pane: 9,
            logical_width: 80.0,
            logical_height: 40.0,
            physical_width: 80,
            physical_height: 40,
            scale: 1.0,
            clip: [0.0, 0.0, 80.0, 40.0],
            visible: true,
            focused: true,
        };
        let facets::PaneContent::Draw {
            packet,
            interaction,
        } = placeholder_content(&pane, true)
        else {
            panic!("fallback must establish visible pane content");
        };
        let packet = Packet::decode(&packet).expect("fallback packet");
        assert_eq!(packet.commands.len(), 2);
        let map = decode_interaction(&interaction).expect("fallback interaction");
        assert!(map.rows_data.is_empty());
        assert_eq!(map.revision, 0);
    }

    #[test]
    fn initial_view_establishes_blank_content_instead_of_no_change() {
        let pane = facets::PaneTarget {
            pane: 9,
            logical_width: 80.0,
            logical_height: 40.0,
            physical_width: 80,
            physical_height: 40,
            scale: 1.0,
            clip: [0.0, 0.0, 80.0, 40.0],
            visible: true,
            focused: true,
        };
        let facets::PaneContent::Draw {
            packet,
            interaction,
        } = placeholder_content(&pane, false)
        else {
            panic!("startup must establish concrete pane content");
        };
        let packet = Packet::decode(&packet).expect("startup packet");
        assert_eq!(packet.commands.len(), 1);
        let map = decode_interaction(&interaction).expect("startup interaction");
        assert_eq!(map.revision, 0);
        assert!(map.rows_data.is_empty());
    }

    #[test]
    fn actor_rejects_stale_frame_mouse_intent() {
        let _fixture = crate::TEST_STATE.lock().expect("terminal state fixture");
        let mut state = UiState::new(2, 7, "terminal-ui-b2-s7");
        state.latest = Some(Arc::new(Snapshot {
            revision: 8,
            input_sequence: 0,
            rows: 2,
            cols: 4,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: false,
            mouse_tracking: 3,
            mouse_sgr: true,
            exited: false,
            exit_code: None,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            viewport_offset: 0,
            max_viewport_offset: 0,
            rows_data: vec![Vec::new(), Vec::new()],
        }));
        *STATE.lock().expect("state") = Some(state);
        let stale = ActorIntent::Mouse {
            revision: 7,
            tracking: 3,
            sgr: true,
            point: CellPoint { row: 0, col: 0 },
            button: 0,
            pressed: true,
            modifiers: 0,
        };
        assert!(handle_input(vec![encode_actor_intent(&stale).expect("intent")]).is_empty());
        STATE.lock().expect("state").take();
    }

    #[test]
    fn high_escape_paste_intent_is_compact_and_roundtrips_raw_utf8() {
        let text = "\u{0008}".repeat(256 * 1024);
        let events = encode_actor_events(
            vec![Intent::Paste {
                text: text.clone(),
                bracketed: true,
            }],
            1024 * 1024,
            16,
        )
        .expect("compact paste events");
        assert_eq!(events.len(), 9);
        assert!(events.iter().all(|event| event.len() <= 32 * 1024));
        let decoded = events
            .iter()
            .map(|event| decode_actor_intent(event).expect("paste chunk"))
            .collect::<Vec<_>>();
        let mut roundtrip = String::new();
        for (index, intent) in decoded.into_iter().enumerate() {
            let ActorIntent::PasteChunk {
                text: chunk,
                bracketed,
                first,
                last,
            } = intent
            else {
                panic!("expected paste chunk");
            };
            assert!(bracketed);
            assert_eq!(first, index == 0);
            assert_eq!(last, index == events.len() - 1);
            roundtrip.push_str(&chunk);
        }
        assert_eq!(roundtrip, text);
    }

    #[test]
    fn chunked_paste_has_one_ordered_bracket_lifetime() {
        let _fixture = crate::TEST_STATE.lock().expect("terminal state fixture");
        let text = "界\\\"\u{0008}".repeat(64);
        let events = encode_actor_events(
            vec![Intent::Paste {
                text: text.clone(),
                bracketed: true,
            }],
            64,
            16,
        )
        .expect("chunked paste events");
        assert!(events.len() > 1);
        let chunks = events
            .iter()
            .map(|event| decode_actor_intent(event).expect("paste chunk"))
            .collect::<Vec<_>>();
        let mut decoded = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let ActorIntent::PasteChunk {
                text,
                bracketed,
                first,
                last,
            } = chunk
            else {
                panic!("unexpected actor intent");
            };
            assert!(*bracketed);
            assert_eq!(*first, index == 0);
            assert_eq!(*last, index + 1 == chunks.len());
            decoded.push_str(text);
        }
        assert_eq!(decoded, text);

        let mut state = UiState::new(2, 7, "terminal-ui-b2-s7");
        state.pane = 9;
        state.latest = Some(Arc::new(Snapshot {
            revision: 1,
            input_sequence: 0,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            application_cursor: false,
            bracketed_paste: true,
            mouse_tracking: 0,
            mouse_sgr: false,
            exited: false,
            exit_code: None,
            chunks_read: 0,
            bytes_read: 0,
            resizes: 0,
            viewport_offset: 0,
            max_viewport_offset: 0,
            rows_data: vec![Vec::new()],
        }));
        *STATE.lock().expect("state") = Some(state);
        let commands = handle_input(events);
        assert!(
            matches!(commands.as_slice(), [Command::SendSessionMessage(message)] if message.body == b"B")
        );
        let mut control = vec![b'C'];
        control.extend_from_slice(&11_u64.to_le_bytes());
        control.extend_from_slice(&3_u64.to_le_bytes());
        control.extend_from_slice(&40_u64.to_le_bytes());
        let mut payload = Vec::new();
        loop {
            let commands = STATE
                .lock()
                .expect("state")
                .as_mut()
                .expect("ui state")
                .accept_input_control(&control);
            let Some(body) = commands.into_iter().find_map(|command| match command {
                Command::SendSessionMessage(message) if message.body.first() == Some(&b'J') => {
                    Some(message.body)
                }
                _ => None,
            }) else {
                break;
            };
            assert!(body.len() <= RELIABLE_INPUT_CHUNK_BYTES + 34);
            payload.extend_from_slice(&body[34..]);
            control = vec![b'K'];
            control.extend_from_slice(&body[9..33]);
        }
        assert_eq!(
            payload
                .strip_prefix(b"\x1b[200~")
                .and_then(|bytes| bytes.strip_suffix(b"\x1b[201~")),
            Some(text.as_bytes())
        );
        assert_eq!(
            payload
                .windows(6)
                .filter(|window| *window == b"\x1b[200~")
                .count(),
            1
        );
        assert_eq!(
            payload
                .windows(6)
                .filter(|window| *window == b"\x1b[201~")
                .count(),
            1
        );
        STATE.lock().expect("state").take();
    }
}
