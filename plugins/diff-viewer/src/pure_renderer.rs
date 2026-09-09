use super::*;
use brood_plugin_sdk::{Facets, facet_codec, facets};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Sequence {
    document: u64,
    refresh_pressed: bool,
    refresh_hovered: bool,
    scrollbar_hovered: bool,
    drag_offset: Option<f32>,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Intent {
    Refresh,
    ScrollBy { document: u64, delta: f32 },
    ScrollTo { document: u64, scroll: f32 },
}
#[derive(Serialize, Deserialize)]
struct Interaction {
    document: u64,
    refresh: [f32; 4],
    loading: bool,
    bar_hit: [f32; 4],
    bar: Option<ScrollbarGeometry>,
    max_scroll: f32,
    viewport: f32,
}
static PUBLICATION: Mutex<(u64, Vec<u8>)> = Mutex::new((0, Vec::new()));

const VIEW_MAGIC: &[u8; 4] = b"BDV1";

fn put_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value.min(u32::MAX as usize) as u32).to_le_bytes());
}
fn put_string(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len());
    out.extend_from_slice(value.as_bytes());
}
fn encode_view(state: &ViewerState) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + state.document.rows.len() * 12);
    out.extend_from_slice(VIEW_MAGIC);
    out.extend_from_slice(&state.document_revision.to_le_bytes());
    out.extend_from_slice(&state.scroll.to_bits().to_le_bytes());
    put_u32(&mut out, state.document.files);
    put_u32(&mut out, state.document.additions);
    put_u32(&mut out, state.document.deletions);
    out.push(u8::from(state.document.truncated));
    out.push(u8::from(state.loading));
    match &state.error {
        Some(error) => {
            out.push(1);
            put_string(&mut out, error);
        }
        None => out.push(0),
    }
    put_u32(&mut out, state.document.rows.len());
    for row in &state.document.rows {
        match row {
            DisplayRow::File {
                path,
                additions,
                deletions,
            } => {
                out.push(0);
                put_u32(&mut out, *additions);
                put_u32(&mut out, *deletions);
                put_string(&mut out, path);
            }
            DisplayRow::Hunk(text) => {
                out.push(1);
                put_string(&mut out, text);
            }
            DisplayRow::Code {
                old_line,
                new_line,
                left,
                right,
                kind,
            } => {
                out.push(2);
                out.push(u8::from(*kind == ChangeKind::Changed));
                out.extend_from_slice(&old_line.unwrap_or(0).to_le_bytes());
                out.extend_from_slice(&new_line.unwrap_or(0).to_le_bytes());
                match left {
                    Some(s) => {
                        out.push(1);
                        put_string(&mut out, s);
                    }
                    None => out.push(0),
                }
                match right {
                    Some(s) => {
                        out.push(1);
                        put_string(&mut out, s);
                    }
                    None => out.push(0),
                }
            }
            DisplayRow::Gap => out.push(3),
        }
    }
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> &'a [u8] {
        let end = self
            .at
            .checked_add(len)
            .filter(|e| *e <= self.bytes.len())
            .expect("diff view bounds");
        let value = &self.bytes[self.at..end];
        self.at = end;
        value
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().expect("u32"))
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().expect("u64"))
    }
    fn string(&mut self) -> &'a str {
        let len = self.u32() as usize;
        std::str::from_utf8(self.take(len)).expect("diff utf8")
    }
    fn maybe_string(&mut self) -> Option<&'a str> {
        (self.u8() != 0).then(|| self.string())
    }
}

struct ViewHeader {
    scroll: f32,
    document_revision: u64,
    files: usize,
    additions: usize,
    deletions: usize,
    truncated: bool,
    loading: bool,
    error: Option<String>,
    rows: usize,
    row_offset: usize,
}
fn decode_header(bytes: &[u8]) -> ViewHeader {
    let mut c = Cursor { bytes, at: 0 };
    assert_eq!(c.take(4), VIEW_MAGIC, "diff view version");
    let document_revision = c.u64();
    let scroll = f32::from_bits(c.u32());
    let files = c.u32() as usize;
    let additions = c.u32() as usize;
    let deletions = c.u32() as usize;
    let truncated = c.u8() != 0;
    let loading = c.u8() != 0;
    let error = c.maybe_string().map(str::to_owned);
    let rows = c.u32() as usize;
    ViewHeader {
        scroll,
        document_revision,
        files,
        additions,
        deletions,
        truncated,
        loading,
        error,
        rows,
        row_offset: c.at,
    }
}

fn decode_row(c: &mut Cursor<'_>, materialize: bool) -> (Option<DisplayRow>, f32) {
    let tag = c.u8();
    let height = match tag {
        0 => FILE_HEADER_HEIGHT,
        1 => HUNK_HEADER_HEIGHT,
        2 => CODE_ROW_HEIGHT,
        3 => FILE_GAP_HEIGHT,
        _ => panic!("diff row tag"),
    };
    let row = match tag {
        0 => {
            let additions = c.u32() as usize;
            let deletions = c.u32() as usize;
            let path = c.string();
            materialize.then(|| DisplayRow::File {
                path: path.to_owned(),
                additions,
                deletions,
            })
        }
        1 => {
            let text = c.string();
            materialize.then(|| DisplayRow::Hunk(text.to_owned()))
        }
        2 => {
            let kind = if c.u8() != 0 {
                ChangeKind::Changed
            } else {
                ChangeKind::Context
            };
            let old = c.u32();
            let new = c.u32();
            let left = c.maybe_string();
            let right = c.maybe_string();
            materialize.then(|| DisplayRow::Code {
                old_line: (old != 0).then_some(old),
                new_line: (new != 0).then_some(new),
                left: left.map(str::to_owned),
                right: right.map(str::to_owned),
                kind,
            })
        }
        3 => materialize.then_some(DisplayRow::Gap),
        _ => panic!("diff row tag"),
    };
    (row, height)
}
pub(super) fn publish() {
    let state = STATE.lock().expect("diff state");
    if state.pane == 0 {
        return;
    }
    let bytes = encode_view(&state);
    drop(state);
    let mut last = PUBLICATION.lock().expect("diff publication");
    if last.1 == bytes {
        return;
    }
    let revision = last.0.checked_add(1).expect("diff revision exhausted");
    if brood_plugin_sdk::publication::publish(revision, &bytes).is_err() {
        // Keep the last accepted root and retry the current view next turn.
        return;
    }
    *last = (revision, bytes);
}
pub(super) fn handle_input(events: Vec<Vec<u8>>) -> Vec<ActorCommand> {
    let mut state = STATE.lock().expect("diff state");
    let mut commands = Vec::new();
    for bytes in events {
        match serde_json::from_slice::<Intent>(&bytes) {
            Ok(Intent::Refresh) => commands.extend(state.begin_refresh()),
            Ok(Intent::ScrollBy { document, delta })
                if document == state.document_revision && delta.is_finite() =>
            {
                let scroll = state.scroll + delta;
                state.set_scroll(scroll);
            }
            Ok(Intent::ScrollTo { document, scroll })
                if document == state.document_revision && scroll.is_finite() =>
            {
                state.set_scroll(scroll);
            }
            Ok(_) => {}
            Err(error) => state.error = Some(format!("Invalid diff intent: {error}")),
        }
    }
    commands
}
fn limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 4096,
    }
}
fn contains(r: [f32; 4], x: f64, y: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= f64::from(r[0])
        && x < f64::from(r[0] + r[2])
        && y >= f64::from(r[1])
        && y < f64::from(r[1] + r[3])
}
fn reduce(
    map: &Interaction,
    sequence: &mut Sequence,
    event: facets::InputEvent,
) -> (Vec<Intent>, Vec<facets::HostRequest>) {
    let mut requests = Vec::new();
    if sequence.document != map.document {
        if sequence.drag_offset.is_some() {
            requests.push(facets::HostRequest::ReleaseCapture);
        }
        *sequence = Sequence {
            document: map.document,
            ..Sequence::default()
        };
    }
    let mut intents = Vec::new();
    let scroll_from_pointer = |y: f64, offset: f32| {
        map.bar.map(|bar| {
            let travel = (bar.track_height - bar.thumb_height).max(0.0);
            if travel > 0.0 {
                ((y as f32 - bar.track_top - offset) / travel).clamp(0.0, 1.0) * map.max_scroll
            } else {
                0.0
            }
        })
    };
    match event {
        facets::InputEvent::PointerMotion { x, y } => {
            sequence.refresh_hovered = !map.loading && contains(map.refresh, x, y);
            sequence.scrollbar_hovered = map.bar.is_some() && contains(map.bar_hit, x, y);
            if let Some(scroll) = sequence
                .drag_offset
                .and_then(|offset| scroll_from_pointer(y, offset))
            {
                intents.push(Intent::ScrollTo {
                    document: map.document,
                    scroll,
                });
            }
        }
        facets::InputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
            ..
        } if button == LEFT_BUTTON => {
            let refresh = !map.loading && contains(map.refresh, x, y);
            sequence.refresh_hovered = refresh;
            if pressed {
                sequence.refresh_pressed = refresh;
                if !refresh
                    && contains(map.bar_hit, x, y)
                    && let Some(bar) = map.bar
                {
                    let offset = if y >= f64::from(bar.thumb_y)
                        && y <= f64::from(bar.thumb_y + bar.thumb_height)
                    {
                        y as f32 - bar.thumb_y
                    } else {
                        bar.thumb_height * 0.5
                    };
                    sequence.drag_offset = Some(offset);
                    sequence.scrollbar_hovered = true;
                    requests.push(facets::HostRequest::Capture);
                    if let Some(scroll) = scroll_from_pointer(y, offset) {
                        intents.push(Intent::ScrollTo {
                            document: map.document,
                            scroll,
                        });
                    }
                }
            } else {
                if sequence.refresh_pressed && refresh {
                    intents.push(Intent::Refresh);
                }
                sequence.refresh_pressed = false;
                if sequence.drag_offset.take().is_some() {
                    requests.push(facets::HostRequest::ReleaseCapture);
                }
            }
        }
        facets::InputEvent::PointerScroll { vertical, .. } => intents.push(Intent::ScrollBy {
            document: map.document,
            delta: -vertical as f32,
        }),
        facets::InputEvent::Key {
            logical_key,
            pressed: true,
            repeat: false,
            ..
        } => {
            let delta = match logical_key.as_str() {
                "ArrowDown" | "j" => Some(CODE_ROW_HEIGHT * 3.0),
                "ArrowUp" | "k" => Some(-CODE_ROW_HEIGHT * 3.0),
                "PageDown" | " " => Some(map.viewport * 0.82),
                "PageUp" => Some(-map.viewport * 0.82),
                _ => None,
            };
            if let Some(delta) = delta {
                intents.push(Intent::ScrollBy {
                    document: map.document,
                    delta,
                });
            }
            match logical_key.as_str() {
                "Home" => intents.push(Intent::ScrollTo {
                    document: map.document,
                    scroll: 0.0,
                }),
                "End" => intents.push(Intent::ScrollTo {
                    document: map.document,
                    scroll: map.max_scroll,
                }),
                "r" | "R" if !map.loading => intents.push(Intent::Refresh),
                _ => {}
            }
        }
        facets::InputEvent::FocusLost
        | facets::InputEvent::CaptureLost
        | facets::InputEvent::PointerLeave => {
            if sequence.drag_offset.take().is_some() {
                requests.push(facets::HostRequest::ReleaseCapture);
            }
            sequence.refresh_pressed = false;
            sequence.refresh_hovered = false;
            sequence.scrollbar_hovered = false;
        }
        _ => {}
    }
    requests.push(facets::HostRequest::Render);
    (intents, requests)
}
impl Facets for DiffViewer {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(&input, limits(input.len()))
            .expect("diff render request");
        let header = decode_header(&request.application_view);
        let sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let width = pane.logical_width as f32;
                let height = pane.logical_height as f32;
                let sequence = if sequence.document == header.document_revision {
                    sequence.clone()
                } else {
                    Sequence::default()
                };
                let viewport = (height - HEADER_HEIGHT - FOOTER_HEIGHT).max(1.0);
                let mut cursor = Cursor {
                    bytes: &request.application_view,
                    at: header.row_offset,
                };
                let mut content_height = 0.0;
                let mut row_origin = 0.0;
                let mut visible = Vec::new();
                for _ in 0..header.rows {
                    let row_height = match cursor.bytes[cursor.at] {
                        0 => FILE_HEADER_HEIGHT,
                        1 => HUNK_HEADER_HEIGHT,
                        2 => CODE_ROW_HEIGHT,
                        3 => FILE_GAP_HEIGHT,
                        _ => panic!("diff row tag"),
                    };
                    let materialize = content_height + row_height >= header.scroll
                        && content_height <= header.scroll + viewport;
                    let (row, decoded_height) = decode_row(&mut cursor, materialize);
                    if let Some(row) = row {
                        if visible.is_empty() {
                            row_origin = content_height;
                        }
                        visible.push(row);
                    }
                    content_height += decoded_height;
                }
                let view = RenderView {
                    width,
                    height,
                    scroll: header.scroll,
                    document: DiffDocument {
                        rows: visible,
                        files: header.files,
                        additions: header.additions,
                        deletions: header.deletions,
                        truncated: header.truncated,
                    },
                    document_revision: header.document_revision,
                    loading: header.loading,
                    error: header.error.clone(),
                    scrollbar_hovered: sequence.scrollbar_hovered || sequence.drag_offset.is_some(),
                    refresh_hovered: sequence.refresh_hovered,
                    refresh_pressed: sequence.refresh_pressed,
                };
                let mut state = ViewerState::new();
                state.width = width;
                state.height = height;
                state.scroll = header.scroll;
                let bar = scrollbar_geometry_for_content(&state, content_height);
                let rect = refresh_button_rect(&state);
                let map = Interaction {
                    document: header.document_revision,
                    refresh: [rect.x, rect.y, rect.width, rect.height],
                    loading: header.loading,
                    bar_hit: [
                        width - SCROLLBAR_HIT_WIDTH,
                        HEADER_HEIGHT,
                        SCROLLBAR_HIT_WIDTH,
                        state.viewport_height(),
                    ],
                    bar,
                    max_scroll: (content_height - state.viewport_height()).max(0.0),
                    viewport: state.viewport_height(),
                };
                let packet = build_packet_window(
                    &view,
                    (width, height),
                    &mut TextLayoutCache::new(pane.pane),
                    content_height,
                    row_origin,
                )
                .expect("diff packet");
                facets::PaneOutput {
                    pane: pane.pane,
                    content: facets::PaneContent::Draw {
                        packet: packet.encode().expect("encode diff"),
                        interaction: serde_json::to_vec(&map).expect("diff interaction"),
                    },
                }
            })
            .collect();
        let response = facets::RenderResponse {
            token: request.token,
            panes,
            next_frame_ns: None,
        };
        facet_codec::encode_render_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded diff response")
    }
    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(&input, limits(input.len()))
            .expect("diff input request");
        let map =
            serde_json::from_slice::<Interaction>(&request.interaction).expect("diff interaction");
        let mut sequence =
            serde_json::from_slice::<Sequence>(&request.sequence_state).unwrap_or_default();
        let (events, requests) = reduce(&map, &mut sequence, request.event);
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: serde_json::to_vec(&sequence).expect("diff sequence"),
            events: events
                .into_iter()
                .map(|event| serde_json::to_vec(&event).expect("diff intent"))
                .collect(),
            requests,
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded diff input")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map() -> Interaction {
        Interaction {
            document: 7,
            refresh: [190.0, 10.0, 94.0, 32.0],
            loading: false,
            bar_hit: [304.0, 52.0, 16.0, 400.0],
            bar: Some(ScrollbarGeometry {
                track_top: 52.0,
                track_height: 400.0,
                thumb_y: 52.0,
                thumb_height: 80.0,
            }),
            max_scroll: 1600.0,
            viewport: 400.0,
        }
    }
    fn button(x: f64, y: f64, pressed: bool) -> facets::InputEvent {
        facets::InputEvent::PointerButton {
            x,
            y,
            button: LEFT_BUTTON,
            pressed,
            modifiers: 0,
        }
    }
    #[test]
    fn scrollbar_uses_right_edge_and_capture_cancels_on_document_change() {
        let mut state = Sequence::default();
        let (events, requests) = reduce(&map(), &mut state, button(310.0, 62.0, true));
        assert!(requests.contains(&facets::HostRequest::Capture));
        assert_eq!(state.drag_offset, Some(10.0));
        assert_eq!(
            events,
            vec![Intent::ScrollTo {
                document: 7,
                scroll: 0.0
            }]
        );
        let (events, _) = reduce(
            &map(),
            &mut state,
            facets::InputEvent::PointerMotion { x: 310.0, y: 382.0 },
        );
        assert_eq!(
            events,
            vec![Intent::ScrollTo {
                document: 7,
                scroll: 1600.0
            }]
        );
        let mut replacement = map();
        replacement.document = 8;
        let (events, requests) = reduce(
            &replacement,
            &mut state,
            facets::InputEvent::PointerMotion { x: 310.0, y: 382.0 },
        );
        assert!(events.is_empty());
        assert!(requests.contains(&facets::HostRequest::ReleaseCapture));
        let (_, requests) = reduce(&map(), &mut Sequence::default(), button(60.0, 62.0, true));
        assert!(!requests.contains(&facets::HostRequest::Capture));
    }
    #[test]
    fn keyboard_and_scroll_preserve_direction_and_pinned_viewport() {
        let mut state = Sequence::default();
        let key = |key: &str| facets::InputEvent::Key {
            code: 0,
            logical_key: key.into(),
            modifiers: 0,
            pressed: true,
            repeat: false,
        };
        assert_eq!(
            reduce(&map(), &mut state, key("PageDown")).0,
            vec![Intent::ScrollBy {
                document: 7,
                delta: 328.0
            }]
        );
        assert_eq!(
            reduce(&map(), &mut state, key("End")).0,
            vec![Intent::ScrollTo {
                document: 7,
                scroll: 1600.0
            }]
        );
        assert_eq!(
            reduce(&map(), &mut state, key("Home")).0,
            vec![Intent::ScrollTo {
                document: 7,
                scroll: 0.0
            }]
        );
        assert_eq!(
            reduce(&map(), &mut state, key("r")).0,
            vec![Intent::Refresh]
        );
        assert_eq!(
            reduce(
                &map(),
                &mut state,
                facets::InputEvent::PointerScroll {
                    x: 0.0,
                    y: 0.0,
                    horizontal: 0.0,
                    vertical: 2.5,
                    modifiers: 0
                }
            )
            .0,
            vec![Intent::ScrollBy {
                document: 7,
                delta: -2.5
            }]
        );
    }
    #[test]
    fn refresh_requires_matching_primary_release_and_cancels_on_focus_loss() {
        let mut state = Sequence::default();
        reduce(&map(), &mut state, button(210.0, 20.0, true));
        let first = reduce(&map(), &mut state.clone(), button(210.0, 20.0, false));
        let retry = reduce(&map(), &mut state.clone(), button(210.0, 20.0, false));
        assert_eq!(first, retry);
        assert_eq!(first.0, vec![Intent::Refresh]);
        reduce(&map(), &mut state, facets::InputEvent::FocusLost);
        assert!(
            reduce(&map(), &mut state, button(210.0, 20.0, false))
                .0
                .is_empty()
        );
        let mut state = Sequence::default();
        let mut right = button(210.0, 20.0, true);
        if let facets::InputEvent::PointerButton { button, .. } = &mut right {
            *button = 0x111;
        }
        reduce(&map(), &mut state, right);
        assert!(
            reduce(&map(), &mut state, button(210.0, 20.0, false))
                .0
                .is_empty()
        );
    }

    #[test]
    fn large_publication_materializes_only_the_visible_tail_rows() {
        let mut state = ViewerState::new();
        state.document_revision = 9;
        state.document.rows = (0..48_000)
            .map(|line| DisplayRow::Code {
                old_line: Some(line + 1),
                new_line: Some(line + 1),
                left: Some("a moderately long source line".into()),
                right: Some("a moderately long source line".into()),
                kind: ChangeKind::Context,
            })
            .collect();
        state.scroll = CODE_ROW_HEIGHT * 47_970.0;
        let bytes = encode_view(&state);
        let header = decode_header(&bytes);
        let mut cursor = Cursor {
            bytes: &bytes,
            at: header.row_offset,
        };
        let viewport = 600.0;
        let mut y = 0.0;
        let mut materialized = Vec::new();
        for _ in 0..header.rows {
            let height = CODE_ROW_HEIGHT;
            let visible = y + height >= header.scroll && y <= header.scroll + viewport;
            let (row, decoded_height) = decode_row(&mut cursor, visible);
            materialized.extend(row);
            y += decoded_height;
        }
        assert_eq!(cursor.at, bytes.len());
        assert_eq!(y, CODE_ROW_HEIGHT * 48_000.0);
        assert!(materialized.len() <= 32, "only viewport rows are decoded");
        assert!(materialized.len() >= 29, "viewport remains fully populated");
    }
}
