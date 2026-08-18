use std::{collections::BTreeMap, sync::Mutex};

use brood_graphics::{Color, Command, Packet, Rect, theme::ashen};
use brood_plugin_sdk::{
    bindings,
    exports::brood::plugin::actor::{
        Command as ActorCommand, Event, Guest, LifecycleEvent, LocalActorMessage,
        PresentationEvent, SessionEvent,
    },
    graphics::UiFrame,
};
use serde::Deserialize;

const STATUS_BAR_ACTOR_ID: &str = "status-bar";
const SHELL_ACTOR_ID: &str = "session-switcher";

const SURFACE: Color = ashen::SURFACE;
const BORDER: Color = ashen::BORDER;
const TEXT: Color = ashen::TEXT;

#[derive(Default)]
struct StatusBackend {
    session_count: usize,
    active_session: Option<String>,
}

struct StatusBarState {
    pane: u64,
    width: f32,
    height: f32,
    backends: BTreeMap<u64, StatusBackend>,
    text_resources: Vec<u64>,
    error: String,
    notice: String,
}

impl StatusBarState {
    const fn new() -> Self {
        Self {
            pane: 0,
            width: 1440.0,
            height: 34.0,
            backends: BTreeMap::new(),
            text_resources: Vec::new(),
            error: String::new(),
            notice: String::new(),
        }
    }
}

static STATE: Mutex<StatusBarState> = Mutex::new(StatusBarState::new());

struct StatusBar;

impl Guest for StatusBar {
    fn dispatch(event: Event) -> Vec<ActorCommand> {
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
    if let Err(error) = redraw(state, resize_epoch) {
        state.error = error;
    }
}

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
        frame.submit(resize_epoch, &packet, &mut state.text_resources)?;
        return Err(format!("layout status bar text: {error}"));
    }
    frame.submit(resize_epoch, &packet, &mut state.text_resources)?;
    state.error.clear();
    Ok(())
}

fn status_bar_text(state: &StatusBarState, width: f32) -> String {
    if !state.notice.is_empty() {
        return state.notice.clone();
    }
    let session_count = state
        .backends
        .values()
        .map(|backend| backend.session_count)
        .sum::<usize>();
    let active = state
        .backends
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
}

bindings::export!(StatusBar with_types_in bindings);
