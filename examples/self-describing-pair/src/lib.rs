//! Paired guest exercising the public versioned registration and message contract.
wit_bindgen::generate!({ path: "wit", world: "brood-plugin" });
use exports::brood::plugin::actor::{Command, Event, Guest, LifecycleEvent};
use brood_package_contract::{VERSION, services::{self, Registration, Request, Role}};
struct ExternalPair;
fn send(binding: String, message: &str) -> Vec<Command> {
    vec![Command::SendHostMessage(Request::Send { version: VERSION, binding, payload: serde_json::json!({"version": 1, "message": message}) }.encode().unwrap())]
}
impl Guest for ExternalPair {
    fn dispatch(event: Event) -> Vec<Command> {
        match event {
            Event::Lifecycle(LifecycleEvent::Start(context)) if context.data.iter().any(|entry| entry.key == "realm" && entry.value == "daemon") => vec![Command::LifecycleComplete(Ok(()))],
            Event::Lifecycle(LifecycleEvent::Start(_)) if cfg!(feature = "fail-start") => vec![Command::LifecycleComplete(Err("intentional replacement startup failure".into()))],
            Event::Lifecycle(LifecycleEvent::Start(_)) => {
                let _retained_timer = brood::plugin::host::timer_create(60_000).unwrap();
                vec![Command::LifecycleComplete(Ok(())), Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 1, query: serde_json::json!({"version": 1, "collection": "instances"}) }.encode().unwrap())] },
            Event::Lifecycle(_) => vec![Command::LifecycleComplete(Ok(()))],
            Event::Mailbox(message) if message.sender == services::GUEST_SENDER => match services::Event::decode(&message.body) {
                Ok(services::Event::Inspection { request_id: 1, response }) => {
                    assert!(response.error.is_none());
                    assert_eq!(response.data.len(), 1, "query crossed a realm or session boundary");
                    assert!(response.data[0]["resources_observed_at_unix_ms"].is_u64());
                    assert!(response.data[0]["resources"]["streams"].is_u64());
                    assert_eq!(response.data[0]["resources"]["timers"], 1);
                    assert_eq!(response.data[0]["resource_assessment"]["timers"]["status"], "within-expectation");
                    assert_eq!(response.data[0]["resource_assessment"]["memory-bytes"]["status"], "unsupported-observation");
                    assert!(response.data[0]["declaration"]["identity"]["knowledge_digest"].is_string());
                    let identity = &response.data[0]["declaration"]["identity"];
                    vec![Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 5, query: serde_json::json!({"version": 1, "collection": "package-documents", "package": identity["package"], "installation": identity["installation"], "document": "external-pair.overview", "limit": 1}) }.encode().unwrap())]
                }
                Ok(services::Event::Inspection { request_id: 5, response }) => {
                    assert!(response.error.is_none());
                    assert_eq!(response.data[0]["id"], "external-pair.overview");
                    assert_eq!(response.data[0]["references"], serde_json::json!(["manifest"]));
                    assert_eq!(response.data[0]["schemas"].as_array().unwrap().len(), 2);
                    assert_eq!(response.data[0]["authority"], "verified-installed-knowledge");
                    assert!(response.data[0]["identity"]["knowledge_digest"].is_string());
                    vec![Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 2, query: serde_json::json!({"version": 1, "collection": "endpoints"}) }.encode().unwrap())]
                }
                Ok(services::Event::Inspection { request_id: 2, response }) => {
                    assert_eq!(response.error.unwrap().code, "observation-unavailable");
                    vec![
                        Command::SendHostMessage(Request::ReportDiagnostic { version: VERSION, id: "org.example.external-pair.health".into(), schema_version: 1, value: Some(serde_json::json!({"ready": true})), failure: None }.encode().unwrap()),
                        Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 3, query: serde_json::json!({"version": 1, "collection": "diagnostics"}) }.encode().unwrap())]
                }
                Ok(services::Event::Inspection { request_id: 3, response }) => {
                    assert!(response.error.is_none());
                    assert_eq!(response.data.len(), 1);
                    assert_eq!(response.data[0]["diagnostic"]["validation"], "accepted");
                    assert_eq!(response.data[0]["diagnostic"]["value"]["ready"], true);
                    vec![
                        Command::SendHostMessage(Request::ReportDiagnostic { version: VERSION, id: "org.example.external-pair.health".into(), schema_version: 1, value: Some(serde_json::json!({"ready": "invalid"})), failure: None }.encode().unwrap()),
                        Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 4, query: serde_json::json!({"version": 1, "collection": "documents", "document": "brood.commands.v1", "limit": 1}) }.encode().unwrap())]
                }
                Ok(services::Event::Inspection { request_id: 4, response }) => {
                    assert!(response.error.is_none());
                    assert_eq!(response.data[0]["id"], "brood.commands.v1");
                    assert_eq!(response.data[0]["category"], "generated-contract");
                    assert!(response.data[0]["content"].as_str().unwrap().contains("name"));
                    vec![Command::SendHostMessage(Request::Inspect { version: VERSION, request_id: 6, query: serde_json::json!({"version": 1, "collection": "documents", "document": "brood.introspection.request.v1", "limit": 1}) }.encode().unwrap())]
                }
                Ok(services::Event::Inspection { request_id: 6, response }) => {
                    assert!(response.error.is_none());
                    assert_eq!(response.data[0]["id"], "brood.introspection.request.v1");
                    assert_eq!(response.data[0]["category"], "generated-contract");
                    assert!(response.data[0]["content"].as_str().unwrap().contains("$schema"));
                    vec![Command::SendHostMessage(Request::Register { version: VERSION, interfaces: vec![Registration { interface: "example.peer".into(), supported_versions: vec![if cfg!(feature = "incompatible") { 2 } else { 1 }] }] }.encode().unwrap())]
                }
                Ok(services::Event::Bound { binding, role: Role::Consumer, .. }) => send(binding, "ping"),
                Ok(services::Event::Message { binding, payload }) if payload["message"] == "ping" => send(binding, "pong"),
                _ => vec![],
            },
            _ => vec![],
        }
    }
    async fn worker_run(_worker: u64, _payload: Vec<u8>) -> Result<Vec<u8>, String> { Ok(vec![]) }
}
export!(ExternalPair with_types_in self);
