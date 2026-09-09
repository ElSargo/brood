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
pub(super) fn publish() {
    let state = STATE.lock().expect("diff state");
    if state.pane == 0 {
        return;
    }
    let bytes = serde_json::to_vec(&state.render_view()).expect("diff view");
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
        let view: RenderView =
            serde_json::from_slice(&request.application_view).expect("diff immutable view");
        let sequence: Sequence =
            serde_json::from_slice(&request.sequence_state).unwrap_or_default();
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let mut view = view.clone();
                view.width = pane.logical_width as f32;
                view.height = pane.logical_height as f32;
                let sequence = if sequence.document == view.document_revision {
                    sequence.clone()
                } else {
                    Sequence::default()
                };
                view.refresh_pressed = sequence.refresh_pressed;
                view.refresh_hovered = sequence.refresh_hovered;
                view.scrollbar_hovered =
                    sequence.scrollbar_hovered || sequence.drag_offset.is_some();
                let mut state = ViewerState::new();
                state.width = view.width;
                state.height = view.height;
                state.document = view.document.clone();
                state.scroll = view.scroll;
                let bar = scrollbar_geometry(&state);
                let rect = refresh_button_rect(&state);
                let map = Interaction {
                    document: view.document_revision,
                    refresh: [rect.x, rect.y, rect.width, rect.height],
                    loading: view.loading,
                    bar_hit: [
                        view.width - SCROLLBAR_HIT_WIDTH,
                        HEADER_HEIGHT,
                        SCROLLBAR_HIT_WIDTH,
                        state.viewport_height(),
                    ],
                    bar,
                    max_scroll: state.max_scroll(),
                    viewport: state.viewport_height(),
                };
                let packet = build_packet(
                    &view,
                    (view.width, view.height),
                    &mut TextLayoutCache::new(pane.pane),
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
}
