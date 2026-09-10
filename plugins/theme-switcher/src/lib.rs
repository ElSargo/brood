//! Presentation-global theme selection through the shared picker.
use brood_plugin_sdk::{
    actor::send_local,
    bindings,
    exports::brood::plugin::actor::{Command, Event, Guest, LifecycleEvent, PresentationEvent},
};
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Default)]
struct Switcher {
    next_request: u64,
    pending: Option<(u64, &'static str)>,
    picker: Option<u64>,
    names: Vec<String>,
}

impl Switcher {
    fn host(&mut self, operation: &'static str, name: Option<&str>) -> Command {
        self.next_request = self
            .next_request
            .checked_add(1)
            .expect("theme request exhausted");
        if operation == "list" || operation == "commit" {
            self.pending = Some((self.next_request, operation));
        }
        Command::SendHostMessage(
            json!({
                "type": "theme", "request": self.next_request, "operation": operation, "name": name,
            })
            .to_string()
            .into_bytes(),
        )
    }

    fn handle(&mut self, sender: &str, message: Value) -> Vec<Command> {
        match message["type"].as_str().unwrap_or_default() {
            "theme-switcher-open"
                if sender == "key-policy" && self.pending.is_none() && self.picker.is_none() =>
            {
                vec![self.host("list", None)]
            }
            "theme-switcher-reset" if sender == "brood.client-init" => {
                self.picker = None;
                self.pending = None;
                vec![self.host("cancel", None)]
            }
            "theme-response" if sender == "key-policy" => {
                let request = message["request"].as_u64();
                let operation = message["operation"].as_str();
                let pending = self
                    .pending
                    .is_some_and(|(id, op)| Some(id) == request && Some(op) == operation);
                if message["success"] != true {
                    if pending {
                        self.pending = None;
                    }
                    return vec![notice(
                        message["message"].as_str().unwrap_or("Theme change failed"),
                    )];
                }
                if !pending {
                    return Vec::new();
                }
                if operation == Some("commit") {
                    self.pending = None;
                    return Vec::new();
                }
                let Some(names) = message["names"].as_array() else {
                    self.pending = None;
                    return Vec::new();
                };
                self.names = names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                // Put the saved choice first so opening preserves the current appearance.
                let current = message["current"].as_str().unwrap_or_default();
                let items: Vec<_> = self
                    .names
                    .iter()
                    .filter(|name| *name == current)
                    .chain(self.names.iter().filter(|name| *name != current))
                    .map(|name| json!({"id": name, "value": name}))
                    .collect();
                self.pending = Some((request.unwrap(), "picker"));
                vec![local(
                    "picker",
                    json!({
                        "type": "picker-create", "request": request,
                        "title": "Themes", "placeholder": "Type to preview · Enter to save",
                        "notify_focus": true, "items": items,
                    }),
                )]
            }
            "picker-created" if sender == "picker" => {
                if self.pending.is_some_and(|(id, op)| {
                    op == "picker" && Some(id) == message["request"].as_u64()
                }) {
                    self.pending = None;
                    self.picker = message["picker"].as_u64();
                }
                Vec::new()
            }
            "picker-focused" | "picker-selected"
                if sender == "picker"
                    && self.picker.is_some()
                    && self.picker == message["picker"].as_u64() =>
            {
                let selected = message["type"] == "picker-selected";
                let name = message["result"].as_str().filter(|name| {
                    message["item_id"].as_str() == Some(*name)
                        && self.names.iter().any(|candidate| candidate == name)
                });
                if selected {
                    self.picker = None;
                    if let Some(name) = name {
                        vec![self.host("commit", Some(name))]
                    } else {
                        vec![
                            self.host("cancel", None),
                            notice("Theme selection is no longer available"),
                        ]
                    }
                } else if name.is_some() || message["item_id"].is_null() {
                    vec![self.host("preview", name)]
                } else {
                    Vec::new()
                }
            }
            "picker-cancelled"
                if sender == "picker"
                    && self.picker.is_some()
                    && self.picker == message["picker"].as_u64() =>
            {
                self.picker = None;
                vec![self.host("cancel", None)]
            }
            "picker-error"
                if sender == "picker"
                    && (self.pending.is_some_and(|(_, op)| op == "picker")
                        || (self.picker.is_some()
                            && self.picker == message["picker"].as_u64())) =>
            {
                self.pending = None;
                let mut commands = Vec::new();
                if let Some(picker) = self.picker.take() {
                    commands.push(local(
                        "picker",
                        json!({"type": "picker-cancel", "picker": picker}),
                    ));
                }
                commands.push(self.host("cancel", None));
                commands.push(notice(
                    message["message"]
                        .as_str()
                        .unwrap_or("Could not open theme picker"),
                ));
                commands
            }
            _ => Vec::new(),
        }
    }
}

fn local(actor: &str, body: Value) -> Command {
    send_local(actor, body.to_string().into_bytes())
}

fn notice(message: &str) -> Command {
    local(
        "status-bar",
        json!({"kind": "status-bar-error", "message": message}),
    )
}

static STATE: Mutex<Option<Switcher>> = Mutex::new(None);
struct ThemeSwitcher;

impl Guest for ThemeSwitcher {
    fn dispatch(event: Event) -> Vec<Command> {
        let mut slot = STATE.lock().expect("theme switcher state");
        let state = slot.get_or_insert_with(Switcher::default);
        match event {
            Event::Lifecycle(LifecycleEvent::Start(_)) => vec![Command::LifecycleComplete(Ok(()))],
            Event::Lifecycle(LifecycleEvent::Stop(_)) => {
                let mut commands = vec![state.host("cancel", None)];
                if let Some(picker) = state.picker.take() {
                    commands.push(local(
                        "picker",
                        json!({"type":"picker-cancel", "picker":picker}),
                    ));
                }
                commands.push(Command::LifecycleComplete(Ok(())));
                commands
            }
            Event::Presentation(PresentationEvent::PaneCreated(created)) => vec![local(
                "session-switcher",
                json!({
                    "kind": "theme-switcher-ready", "pane": created.pane,
                }),
            )],
            Event::Mailbox(message) => serde_json::from_slice(&message.body)
                .map(|body| state.handle(&message.sender, body))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
}

bindings::export!(ThemeSwitcher with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    fn host_body(commands: Vec<Command>) -> Value {
        let Command::SendHostMessage(body) = &commands[0] else {
            panic!("host command expected")
        };
        serde_json::from_slice(body).unwrap()
    }

    fn opened() -> Switcher {
        Switcher {
            picker: Some(7),
            names: vec!["ashen".into(), "github_light".into()],
            ..Switcher::default()
        }
    }

    #[test]
    fn focus_previews_selection_commits_and_late_focus_cannot_revert_selection() {
        let mut state = opened();
        let mut message = json!({"type":"picker-focused", "picker":7, "item_id":"github_light", "result":"github_light"});
        assert!(state.handle("impostor", message.clone()).is_empty());
        assert_eq!(
            host_body(state.handle("picker", message.clone()))["operation"],
            "preview"
        );
        message["type"] = "picker-selected".into();
        assert_eq!(
            host_body(state.handle("picker", message.clone()))["operation"],
            "commit"
        );
        message["type"] = "picker-focused".into();
        assert!(state.handle("picker", message).is_empty());
    }

    #[test]
    fn cancel_and_empty_focus_restore_without_saving_and_stale_ids_are_ignored() {
        let mut state = opened();
        let empty = json!({"type":"picker-focused", "picker":7, "item_id":null, "result":null});
        let request = host_body(state.handle("picker", empty));
        assert_eq!(request["operation"], "preview");
        assert!(request["name"].is_null());
        assert!(
            state
                .handle("picker", json!({"type":"picker-cancelled", "picker":6}))
                .is_empty()
        );
        assert_eq!(
            host_body(state.handle("picker", json!({"type":"picker-cancelled", "picker":7})))["operation"],
            "cancel"
        );
        assert!(state.picker.is_none());
    }

    #[test]
    fn repeated_open_is_ignored_until_correlated_create_reply_arrives() {
        let mut state = Switcher::default();
        let open = json!({"type":"theme-switcher-open"});
        state.handle("key-policy", open.clone());
        assert!(state.handle("key-policy", open.clone()).is_empty());
        let commands = state.handle(
            "key-policy",
            json!({
                "type":"theme-response", "request":1, "operation":"list", "success":true,
                "current":"github_light", "names":["ashen", "github_light"],
            }),
        );
        let Command::SendLocalMessage(message) = &commands[0] else {
            panic!("picker request expected")
        };
        let body: Value = serde_json::from_slice(&message.body).unwrap();
        assert_eq!(body["items"][0]["id"], "github_light");
        assert_eq!(body["notify_focus"], true);
        assert!(state.handle("key-policy", open.clone()).is_empty());
        state.handle(
            "picker",
            json!({"type":"picker-created", "request":99, "picker":8}),
        );
        assert!(state.picker.is_none());
        state.handle(
            "picker",
            json!({"type":"picker-created", "request":1, "picker":7, "queued":true}),
        );
        assert_eq!(state.picker, Some(7));
        assert!(state.handle("key-policy", open).is_empty());
    }
}
