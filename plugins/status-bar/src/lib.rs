use std::{collections::BTreeMap, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::bindings;
use brood_plugin_sdk::exports::brood::plugin::actor::{
    Command as ActorCommand, Event, Guest, LifecycleEvent, LocalActorMessage, PresentationEvent,
    SessionEvent,
};
#[cfg(not(feature = "pure-renderer"))]
use brood_plugin_sdk::graphics::UiFrame;
#[cfg(feature = "pure-renderer")]
use brood_plugin_sdk::{Facets, export_visual_plugin, facet_codec, facets};
use serde::{Deserialize, Serialize};

const STATUS_BAR_ACTOR_ID: &str = "status-bar";
const SHELL_ACTOR_ID: &str = "session-switcher";

const SURFACE: Color = ashen::SURFACE;
const BORDER: Color = ashen::BORDER;
const TEXT: Color = ashen::TEXT;

#[derive(Clone, Default, Deserialize, Serialize)]
struct StatusBackend {
    session_count: usize,
    active_session: Option<String>,
}

struct StatusBarState {
    pane: u64,
    width: f32,
    height: f32,
    backends: BTreeMap<u64, StatusBackend>,
    #[cfg(not(feature = "pure-renderer"))]
    text_resources: Vec<u64>,
    error: String,
    notice: String,
    #[cfg(feature = "pure-renderer")]
    render_revision: u64,
    #[cfg(feature = "pure-renderer")]
    published_view: Vec<u8>,
}

impl StatusBarState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 1440.0,
            height: 34.0,
            backends: BTreeMap::new(),
            #[cfg(not(feature = "pure-renderer"))]
            text_resources: Vec::new(),
            error: String::new(),
            notice: String::new(),
            #[cfg(feature = "pure-renderer")]
            render_revision: 0,
            #[cfg(feature = "pure-renderer")]
            published_view: Vec::new(),
        }
    }
}

static STATE: Mutex<StatusBarState> = Mutex::new(StatusBarState::new());
#[cfg(feature = "pure-renderer")]
static PENDING_PUBLICATION: Mutex<Option<(u64, Vec<u8>)>> = Mutex::new(None);

#[cfg(feature = "pure-renderer")]
#[derive(Deserialize, Serialize)]
struct StatusRenderView {
    backends: BTreeMap<u64, StatusBackend>,
    error: String,
    notice: String,
}

#[cfg(feature = "pure-renderer")]
struct PublicationTurn;

#[cfg(feature = "pure-renderer")]
impl Drop for PublicationTurn {
    fn drop(&mut self) {
        let mut pending = PENDING_PUBLICATION
            .lock()
            .expect("publication mutex poisoned");
        if let Some((revision, bytes)) = pending.take()
            && brood_plugin_sdk::publication::publish(revision, &bytes).is_err()
        {
            // Retain the newest staged view for the next actor turn.
            *pending = Some((revision, bytes));
        }
    }
}

struct StatusBar;

impl Guest for StatusBar {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
        #[cfg(feature = "pure-renderer")]
        let _publication_turn = PublicationTurn;
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) => {
                if context.instance_id != STATUS_BAR_ACTOR_ID
                    || context.backend_id.is_some()
                    || context.session_id.is_some()
                {
                    return vec![ActorCommand::LifecycleComplete(Err(
                        "status bar must be started as presentation-global state".to_string(),
                    ))];
                }
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                vec![ActorCommand::LifecycleComplete(Ok(()))]
            }
            Event::Presentation(PresentationEvent::PaneCreated(created)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                state.pane = created.pane;
                state.width = created.width.max(240.0);
                state.height = created.height.max(24.0);
                redraw_or_record_error(&mut state);
                vec![ActorCommand::SendLocalMessage(LocalActorMessage {
                    actor: SHELL_ACTOR_ID.to_string(),
                    body: format!(r#"{{"kind":"status-bar-ready","pane":{}}}"#, state.pane)
                        .into_bytes(),
                })]
            }
            Event::Presentation(PresentationEvent::PaneResized(resized)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                if resized.pane == state.pane {
                    state.width = resized.width.max(240.0);
                    state.height = resized.height.max(24.0);
                    redraw_for_resize_or_record_error(&mut state, resized.resize_epoch);
                }
                Vec::new()
            }
            #[cfg(not(feature = "pure-renderer"))]
            Event::Presentation(PresentationEvent::RenderHint(hint)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                if let Err(error) =
                    brood_plugin_sdk::graphics::render_with_hint(&hint, || redraw(&mut state, None))
                {
                    state.error = error;
                }
                Vec::new()
            }
            Event::Session(SessionEvent::Snapshot(snapshot)) => {
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                let active_session = snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == snapshot.active)
                    .map(|session| session.name.clone());
                state.backends.insert(
                    snapshot.backend_id,
                    StatusBackend {
                        session_count: snapshot.sessions.len(),
                        active_session,
                    },
                );
                redraw_or_record_error(&mut state);
                Vec::new()
            }
            Event::Mailbox(message) if message.sender == SHELL_ACTOR_ID => {
                let Ok(notice) = serde_json::from_slice::<StatusBarNotice>(&message.body) else {
                    return Vec::new();
                };
                if notice.kind != "status-bar-error" || notice.message.is_empty() {
                    return Vec::new();
                }
                let Ok(mut state) = STATE.lock() else {
                    return Vec::new();
                };
                state.notice = notice.message;
                redraw_or_record_error(&mut state);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

fn redraw_or_record_error(state: &mut StatusBarState) {
    redraw_for_resize_or_record_error(state, None);
}

fn redraw_for_resize_or_record_error(state: &mut StatusBarState, resize_epoch: Option<u64>) {
    #[cfg(feature = "pure-renderer")]
    let result = {
        let _ = resize_epoch;
        publish_view(state)
    };
    #[cfg(not(feature = "pure-renderer"))]
    let result = if brood_plugin_sdk::graphics::render_is_active() {
        redraw(state, resize_epoch)
    } else {
        brood_plugin_sdk::graphics::request_render_for_resize(state.pane, resize_epoch)
    };
    if let Err(error) = result {
        state.error = error;
    }
}

#[cfg(feature = "pure-renderer")]
fn publish_view(state: &mut StatusBarState) -> Result<(), String> {
    let bytes = serde_json::to_vec(&StatusRenderView {
        backends: state.backends.clone(),
        error: state.error.clone(),
        notice: state.notice.clone(),
    })
    .map_err(|error| format!("serialize status render view: {error}"))?;
    if bytes == state.published_view {
        return Ok(());
    }
    state.render_revision = state.render_revision.saturating_add(1).max(1);
    state.published_view.clone_from(&bytes);
    *PENDING_PUBLICATION
        .lock()
        .map_err(|_| "status publication mutex poisoned".to_string())? =
        Some((state.render_revision, bytes));
    Ok(())
}

#[cfg(not(feature = "pure-renderer"))]
fn redraw(state: &mut StatusBarState, resize_epoch: Option<u64>) -> Result<(), String> {
    if state.pane == 0 {
        return Ok(());
    }
    let mut frame = UiFrame::new(state.pane);
    let mut packet = Packet::new();
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, state.height),
        color: SURFACE,
    });
    packet.push(Command::FillRect {
        rect: Rect::new(0.0, 0.0, state.width, 1.0),
        color: BORDER,
    });
    let text = status_bar_text(state, state.width);
    if let Err(error) = frame.draw_text_clipped(
        &mut packet,
        &text,
        12.0,
        state.width - 24.0,
        16.0,
        TEXT,
        12.0,
        ((state.height - 16.0) * 0.5).max(3.0),
    ) {
        // Establish the pane's canvas even if text shaping is temporarily
        // unavailable. The accepted layout can then resize the pane and a
        // later resize/session event retries the complete packet.
        frame.queue(resize_epoch, &packet, &mut state.text_resources)?;
        return Err(format!("layout status bar text: {error}"));
    }
    frame.queue(resize_epoch, &packet, &mut state.text_resources)?;
    state.error.clear();
    Ok(())
}

#[cfg(any(test, not(feature = "pure-renderer")))]
fn status_bar_text(state: &StatusBarState, width: f32) -> String {
    status_bar_text_parts(&state.backends, &state.notice, width)
}

fn status_bar_text_parts(
    backends: &BTreeMap<u64, StatusBackend>,
    notice: &str,
    width: f32,
) -> String {
    if !notice.is_empty() {
        return notice.to_string();
    }
    let session_count = backends
        .values()
        .map(|backend| backend.session_count)
        .sum::<usize>();
    let active = backends
        .values()
        .find_map(|backend| backend.active_session.as_deref())
        .unwrap_or("No session");
    let session_label = match session_count {
        0 => "No session".to_string(),
        1 => format!("{active} · 1 session"),
        count => format!("{active} · {count} sessions"),
    };
    if width >= 1120.0 {
        format!(
            "{session_label}    Ctrl+Shift+/ Shortcuts    Ctrl+Alt+M Sessions    Ctrl+Alt+I Inbox    Ctrl+Alt+T Terminal"
        )
    } else if width >= 820.0 {
        format!(
            "{session_label}    Ctrl+Shift+/ Shortcuts    Ctrl+Alt+M Sessions    Ctrl+Alt+T Terminal"
        )
    } else if width >= 560.0 {
        format!("{session_label}    Ctrl+Shift+/ Shortcuts    Ctrl+Alt+M Sessions")
    } else {
        "Ctrl+Shift+/ Shortcuts    Ctrl+Alt+M Sessions".to_string()
    }
}

#[cfg(feature = "pure-renderer")]
impl Facets for StatusBar {
    fn render(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_render_request(
            &input,
            facet_codec::DecodeLimits {
                max_wire_bytes: input.len(),
                max_allocated_bytes: input.len().saturating_mul(4).max(4096),
                max_allocations: 1024,
            },
        )
        .expect("valid status render request");
        let view: StatusRenderView =
            serde_json::from_slice(&request.application_view).expect("valid status render view");
        let panes = request
            .panes
            .iter()
            .map(|pane| {
                let width = pane.logical_width as f32;
                let height = pane.logical_height as f32;
                let mut packet = Packet::new();
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, 0.0, width, height),
                    color: SURFACE,
                });
                packet.push(Command::FillRect {
                    rect: Rect::new(0.0, 0.0, width, 1.0),
                    color: BORDER,
                });
                let mut text = brood_plugin_sdk::graphics::TextLayoutCache::new(pane.pane);
                text.begin_frame();
                text.draw_text_clipped(
                    &mut packet,
                    &status_bar_text_parts(&view.backends, &view.notice, width),
                    12.0,
                    width - 24.0,
                    16.0,
                    TEXT,
                    12.0,
                    ((height - 16.0) * 0.5).max(3.0),
                )
                .expect("bounded status text");
                facets::PaneOutput {
                    pane: pane.pane,
                    content: facets::PaneContent::Draw {
                        packet: packet.encode().expect("status packet"),
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
        .expect("bounded status response")
    }

    fn interpret(input: Vec<u8>) -> Vec<u8> {
        let request = facet_codec::decode_input_request(
            &input,
            facet_codec::DecodeLimits {
                max_wire_bytes: input.len(),
                max_allocated_bytes: input.len().saturating_mul(2).max(1024),
                max_allocations: 128,
            },
        )
        .expect("valid status input");
        let response = facets::InputResponse {
            token: request.token,
            next_sequence_state: request.sequence_state,
            events: Vec::new(),
            requests: Vec::new(),
        };
        facet_codec::encode_input_response_owned(
            &response,
            request.limits.max_output_bytes as usize,
        )
        .expect("bounded status input response")
    }
}

#[derive(Deserialize)]
struct StatusBarNotice {
    kind: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_discovery_survives_narrow_status_bars() {
        let mut state = StatusBarState::new();
        assert!(status_bar_text(&state, 360.0).contains("Ctrl+Shift+/ Shortcuts"));
        state.backends.insert(
            1,
            StatusBackend {
                session_count: 2,
                active_session: Some("main".to_string()),
            },
        );
        let wide = status_bar_text(&state, 1440.0);
        assert!(wide.contains("main · 2 sessions"));
        assert!(wide.contains("Ctrl+Alt+M Sessions"));
        assert!(wide.contains("Ctrl+Alt+I Inbox"));
        assert!(wide.contains("Ctrl+Alt+T Terminal"));

        state.backends.get_mut(&1).expect("backend").session_count = 1;
        assert!(status_bar_text(&state, 1440.0).contains("main · 1 session"));

        state.notice = "terminal-ui failed to create its pane".to_string();
        assert_eq!(
            status_bar_text(&state, 360.0),
            "terminal-ui failed to create its pane"
        );
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_codec_roundtrips_deterministically() {
        use brood_plugin_sdk::{Facets, facet_codec, facets};
        let limits = facets::InvocationLimits {
            max_output_bytes: 4096,
            max_packet_bytes: 4096,
            max_interaction_bytes: 256,
            max_sequence_bytes: 256,
            max_events: 4,
            max_event_bytes: 256,
            max_host_requests: 2,
            max_resources: 8,
            max_resource_bytes: 65536,
        };
        let token = facets::RenderToken {
            scope: facets::Scope {
                client_generation: 1,
                actor_generation: 2,
                facet_generation: 3,
            },
            invocation: 4,
            snapshot_revision: 5,
            candidate: 6,
            pane_membership_revision: 7,
            geometry_revision: 8,
            host_configuration_revision: 9,
            renderer_generation: 10,
            interaction_revision: 11,
        };
        let request = facets::RenderRequest {
            token: token.clone(),
            panes: Vec::new(),
            target_time_ns: 0,
            cutoff_ns: 0,
            configuration: Vec::new(),
            application_view: serde_json::to_vec(&super::StatusRenderView {
                backends: BTreeMap::new(),
                error: String::new(),
                notice: String::new(),
            })
            .expect("view"),
            sequence_state: Vec::new(),
            limits,
        };
        let mut wire = vec![0; 4096];
        let len = facet_codec::encode_render_request(&request, &mut wire).expect("encode");
        wire.truncate(len);
        let first = <super::StatusBar as Facets>::render(wire.clone());
        let retry = <super::StatusBar as Facets>::render(wire);
        assert_eq!(first, retry);
        let response = facet_codec::decode_render_response(
            &first,
            facet_codec::DecodeLimits {
                max_wire_bytes: first.len(),
                max_allocated_bytes: 8192,
                max_allocations: 64,
            },
        )
        .expect("decode");
        assert_eq!(response.token, token);
        assert!(response.panes.is_empty());
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn unchanged_view_does_not_advance_publication_revision() {
        let mut state = StatusBarState::new();
        publish_view(&mut state).expect("initial view");
        let revision = state.render_revision;
        PENDING_PUBLICATION.lock().expect("pending").take();
        publish_view(&mut state).expect("unchanged view");
        assert_eq!(state.render_revision, revision);
        assert!(PENDING_PUBLICATION.lock().expect("pending").is_none());
        state.notice = "changed".to_string();
        publish_view(&mut state).expect("changed view");
        assert_eq!(state.render_revision, revision + 1);
    }

    #[cfg(feature = "pure-renderer")]
    #[test]
    fn facet_interpret_is_deterministic_and_preserves_sequence() {
        use brood_plugin_sdk::{Facets, facet_codec, facets};
        let request = facets::InputRequest {
            token: facets::InputToken {
                scope: facets::Scope {
                    client_generation: 1,
                    actor_generation: 2,
                    facet_generation: 3,
                },
                input_sequence: 4,
                frame: 5,
                pane: 6,
                snapshot_revision: 7,
                sequence_revision: 8,
            },
            geometry: facets::PaneTarget {
                pane: 6,
                logical_width: 800.0,
                logical_height: 34.0,
                physical_width: 800,
                physical_height: 34,
                scale: 1.0,
                clip: [0.0, 0.0, 800.0, 34.0],
                visible: true,
                focused: false,
            },
            event: facets::InputEvent::PointerMotion { x: 1.0, y: 1.0 },
            interaction: Vec::new(),
            sequence_state: vec![1, 2, 3],
            limits: facets::InvocationLimits {
                max_output_bytes: 1024,
                max_packet_bytes: 1024,
                max_interaction_bytes: 128,
                max_sequence_bytes: 128,
                max_events: 2,
                max_event_bytes: 128,
                max_host_requests: 2,
                max_resources: 2,
                max_resource_bytes: 1024,
            },
        };
        let mut wire = vec![0; 2048];
        let len = facet_codec::encode_input_request(&request, &mut wire).expect("encode");
        wire.truncate(len);
        let first = <StatusBar as Facets>::interpret(wire.clone());
        assert_eq!(first, <StatusBar as Facets>::interpret(wire));
        let response = facet_codec::decode_input_response(
            &first,
            facet_codec::DecodeLimits {
                max_wire_bytes: first.len(),
                max_allocated_bytes: 4096,
                max_allocations: 64,
            },
        )
        .expect("decode");
        assert_eq!(response.next_sequence_state, vec![1, 2, 3]);
        assert!(response.events.is_empty());
        assert!(response.requests.is_empty());
    }
}

#[cfg(feature = "pure-renderer")]
export_visual_plugin!(StatusBar);
#[cfg(not(feature = "pure-renderer"))]
bindings::export!(StatusBar with_types_in bindings);
