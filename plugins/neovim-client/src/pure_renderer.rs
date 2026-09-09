use super::*;
use brood_plugin_sdk::{Facets, facet_codec, facets};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct View {
    screen: Option<Screen>,
    error: Option<String>,
    status: String,
    runner_ready: bool,
    cell_width: f32,
    line_height: f32,
}

impl View {
    fn from_state(state: &ClientState) -> Self {
        Self {
            screen: state.screen.clone(),
            error: state.error.clone(),
            status: state.status.clone(),
            runner_ready: state.runner_ready,
            cell_width: state.cell_width,
            line_height: state.line_height,
        }
    }
}

// Retains only the last encoded view, never a mutable application reference.
static PUBLICATION: Mutex<(u64, Vec<u8>)> = Mutex::new((0, Vec::new()));
pub(super) fn publish() {
    let Ok(guard) = STATE.lock() else {
        return;
    };
    let Some(state) = guard.as_ref().filter(|state| state.pane != 0) else {
        return;
    };
    let bytes = serde_json::to_vec(&View::from_state(state)).expect("serialize Neovim view");
    drop(guard);
    let mut published = PUBLICATION.lock().expect("publication lock");
    if published.1 == bytes {
        return;
    }
    let revision = published
        .0
        .checked_add(1)
        .expect("Neovim view revision exhausted");
    if brood_plugin_sdk::publication::publish(revision, &bytes).is_err() {
        // Keep the last accepted root and retry the current view next turn.
        return;
    }
    *published = (revision, bytes);
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum Intent {
    Key(String),
    Text(String),
    Paste(String),
    UnsupportedImage,
}

fn interpret_event(event: facets::InputEvent) -> Option<Intent> {
    match event {
        facets::InputEvent::Key {
            logical_key,
            modifiers,
            pressed: true,
            ..
        } => nvim_key(&logical_key, modifiers).map(Intent::Key),
        facets::InputEvent::Text { text }
        | facets::InputEvent::Composition {
            text,
            committed: true,
        } => {
            let text: String = text.chars().filter(|c| !c.is_control()).collect();
            (!text.is_empty()).then_some(Intent::Text(text))
        }
        facets::InputEvent::Paste { text } => (!text.is_empty()).then_some(Intent::Paste(text)),
        facets::InputEvent::Image { .. } => Some(Intent::UnsupportedImage),
        _ => None,
    }
}

pub(super) fn handle_input(events: Vec<Vec<u8>>) -> Vec<Command> {
    let Ok(mut guard) = STATE.lock() else {
        return Vec::new();
    };
    let Some(state) = guard.as_mut() else {
        return Vec::new();
    };
    for event in events {
        let result = match serde_json::from_slice::<Intent>(&event) {
            Ok(Intent::Key(key)) => {
                state.queue_runner_value(&json!({"v":1,"type":"key","key":key}))
            }
            Ok(Intent::Text(text)) => state.queue_text("text", &text),
            Ok(Intent::Paste(text)) => state.queue_paste(text),
            Ok(Intent::UnsupportedImage) => Err("Neovim does not accept image input".into()),
            Err(error) => Err(format!("Invalid Neovim input intent: {error}")),
        };
        if let Err(error) = result {
            state.record_error(error);
        }
    }
    Vec::new()
}

fn decode_limits(len: usize) -> facet_codec::DecodeLimits {
    facet_codec::DecodeLimits {
        max_wire_bytes: len,
        max_allocated_bytes: len.saturating_mul(4).max(4096),
        max_allocations: 16384,
    }
}

impl Facets for NeovimClient {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(&input, decode_limits(input.len()))
            .expect("Neovim render request");
        let view: View =
            serde_json::from_slice(&request.application_view).expect("Neovim immutable view");
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let state = RenderState {
                    pane: pane.pane,
                    width: pane.logical_width as f32,
                    height: pane.logical_height as f32,
                    cell_width: view.cell_width,
                    line_height: view.line_height,
                    focused: pane.focused,
                    screen: view.screen.as_ref(),
                    error: view.error.as_deref(),
                    status: &view.status,
                    runner_ready: view.runner_ready,
                };
                let (packet, _) = build_screen_packet(&state, &mut BTreeMap::new())
                    .expect("Neovim screen packet");
                facets::PaneOutput {
                    pane: pane.pane,
                    content: facets::PaneContent::Draw {
                        packet: packet.encode().expect("encode Neovim packet"),
                        // The current Neovim frontend accepts text/key intents only;
                        // there are no pointer hit regions to reconstruct in the actor.
                        interaction: Vec::new(),
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
        .expect("bounded Neovim response")
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(&input, decode_limits(input.len()))
            .expect("Neovim input request");
        let events = interpret_event(request.event)
            .into_iter()
            .map(|intent| serde_json::to_vec(&intent).expect("Neovim semantic input"))
            .collect();
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: Vec::new(),
            events,
            requests: Vec::new(),
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded Neovim input response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> facets::InvocationLimits {
        facets::InvocationLimits {
            max_output_bytes: 8192,
            max_packet_bytes: 4096,
            max_interaction_bytes: 1024,
            max_sequence_bytes: 1024,
            max_events: 8,
            max_event_bytes: 4096,
            max_host_requests: 4,
            max_resources: 32,
            max_resource_bytes: 65536,
        }
    }
    fn pane() -> facets::PaneTarget {
        facets::PaneTarget {
            pane: 9,
            logical_width: 120.0,
            logical_height: 80.0,
            physical_width: 240,
            physical_height: 160,
            scale: 2.0,
            clip: [0.0, 0.0, 120.0, 80.0],
            visible: true,
            focused: true,
        }
    }
    fn scope() -> facets::Scope {
        facets::Scope {
            client_generation: 1,
            actor_generation: 2,
            facet_generation: 3,
        }
    }
    #[test]
    fn wire_input_retry_preserves_token_and_semantic_key() {
        let request = facets::InputRequest {
            token: facets::InputToken {
                scope: scope(),
                input_sequence: 8,
                frame: 6,
                pane: 9,
                snapshot_revision: 5,
                sequence_revision: 0,
            },
            geometry: pane(),
            event: facets::InputEvent::Key {
                code: 28,
                logical_key: "Enter".into(),
                modifiers: 0,
                pressed: true,
                repeat: false,
            },
            interaction: Vec::new(),
            sequence_state: Vec::new(),
            limits: limits(),
        };
        let mut wire = vec![0; 8192];
        let len = facet_codec::encode_input_request(&request, &mut wire).unwrap();
        wire.truncate(len);
        let first = NeovimClient::interpret(wire.clone());
        assert_eq!(first, NeovimClient::interpret(wire));
        let reply = facet_codec::decode_input_response(&first, decode_limits(first.len())).unwrap();
        assert_eq!(reply.token, request.token);
        assert_eq!(
            serde_json::from_slice::<Intent>(&reply.events[0]).unwrap(),
            Intent::Key("<CR>".into())
        );
    }
    #[test]
    fn render_uses_exact_target_and_pure_view() {
        let screen = Screen {
            revision: 5,
            columns: 10,
            rows: 2,
            lines: vec![String::new(); 2],
            cursor_row: 0,
            cursor_column: 1,
            cursor_visible: true,
            mode: "normal".into(),
            buffer_path: String::new(),
            modified: false,
            readonly: false,
            cmdline: String::new(),
            message: String::new(),
        };
        let view = View {
            screen: Some(screen),
            error: None,
            status: String::new(),
            runner_ready: true,
            cell_width: 8.0,
            line_height: 20.0,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 6);
        assert!(json.get("layouts").is_none());
        assert!(json.get("runner_queue").is_none());
        let request = facets::RenderRequest {
            token: facets::RenderToken {
                scope: scope(),
                invocation: 7,
                snapshot_revision: 5,
                candidate: 3,
                pane_membership_revision: 1,
                geometry_revision: 2,
                host_configuration_revision: 1,
                renderer_generation: 1,
                interaction_revision: 0,
            },
            panes: vec![pane()],
            target_time_ns: 123,
            cutoff_ns: 456,
            configuration: Vec::new(),
            application_view: serde_json::to_vec(&view).unwrap(),
            sequence_state: Vec::new(),
            limits: limits(),
        };
        let mut wire = vec![0; 8192];
        let len = facet_codec::encode_render_request(&request, &mut wire).unwrap();
        wire.truncate(len);
        let first = NeovimClient::render(wire.clone());
        assert_eq!(first, NeovimClient::render(wire));
        let reply =
            facet_codec::decode_render_response(&first, decode_limits(first.len())).unwrap();
        assert_eq!(reply.token, request.token);
        let facets::PaneContent::Draw { packet, .. } = &reply.panes[0].content else {
            panic!("draw");
        };
        let expected = build_screen_packet(
            &RenderState {
                pane: 9,
                width: 120.0,
                height: 80.0,
                cell_width: 8.0,
                line_height: 20.0,
                focused: true,
                screen: view.screen.as_ref(),
                error: None,
                status: "",
                runner_ready: true,
            },
            &mut BTreeMap::new(),
        )
        .unwrap()
        .0
        .encode()
        .unwrap();
        assert_eq!(*packet, expected);
    }

    #[test]
    fn committed_composition_and_paste_preserve_distinct_semantics() {
        assert_eq!(
            interpret_event(facets::InputEvent::Composition {
                text: "日\n本".into(),
                committed: true
            }),
            Some(Intent::Text("日本".into()))
        );
        assert_eq!(
            interpret_event(facets::InputEvent::Composition {
                text: "日".into(),
                committed: false
            }),
            None
        );
        assert_eq!(
            interpret_event(facets::InputEvent::Paste {
                text: "a\nb".into()
            }),
            Some(Intent::Paste("a\nb".into()))
        );
        assert_eq!(interpret_event(facets::InputEvent::FocusLost), None);
    }
}
