use std::path::Path;

use anyhow::{Context, Result, ensure};
use manifold_wasm::{
    ABI_INPUT_MODE_PASSTHROUGH, GlobalKeyActorMessage, GlobalKeyDecision, GlobalKeyPolicyInput,
    GlobalKeyPolicyInvocation, HostMessage, Registry, WasmGlobalKeyPolicy,
};
use serde::Serialize;

pub struct ClientKeyPolicy {
    wasm: Option<WasmGlobalKeyPolicy>,
    publications: Registry,
    publication_actor: manifold_wasm::ActorId,
    _publication_mailbox: manifold_wasm::Mailbox,
    shortcut_passthrough: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct KeyPolicyAppPane {
    pub pane: u64,
    pub app_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct KeyPolicySnapshot {
    pub focused_pane: Option<u64>,
    pub panes: Vec<u64>,
    pub managed_panes: Vec<u64>,
    pub app_panes: Vec<KeyPolicyAppPane>,
}

#[derive(Debug)]
pub struct KeyPolicyEvaluation {
    pub decision: GlobalKeyDecision,
    pub host_messages: Vec<HostMessage>,
    pub actor_messages: Vec<GlobalKeyActorMessage>,
}

impl ClientKeyPolicy {
    pub fn load(spec: Option<&[String]>) -> Result<Self> {
        let wasm = spec.map(load_wasm_key_policy).transpose()?;
        let mut publications = Registry::new();
        let publication_mailbox = publications
            .register_actor("presentation-client-state", 1)
            .context("register presentation-client policy publication source")?;
        let publication_actor = publication_mailbox.actor_id();
        Ok(Self {
            wasm,
            publications,
            publication_actor,
            _publication_mailbox: publication_mailbox,
            shortcut_passthrough: false,
        })
    }

    pub fn evaluate(
        &mut self,
        key_code: i32,
        key_state: i32,
        modifiers: i32,
        repeat: bool,
        snapshot: &KeyPolicySnapshot,
    ) -> Result<KeyPolicyEvaluation> {
        let Some(policy) = self.wasm.as_mut() else {
            return Ok(KeyPolicyEvaluation {
                decision: GlobalKeyDecision::Abstain,
                host_messages: Vec::new(),
                actor_messages: Vec::new(),
            });
        };

        let publication = self.publications.append_default_publication_frame(
            self.publication_actor,
            serde_json::to_vec(snapshot).context("encode client key-policy publication")?,
        )?;
        let input = GlobalKeyPolicyInput::new(
            key_code,
            key_state,
            modifiers,
            ABI_INPUT_MODE_PASSTHROUGH,
            repeat,
        );
        let outcome =
            policy.handle_key(GlobalKeyPolicyInvocation::new(input, Some(&publication)))?;
        let mut host_messages = Vec::new();
        let mut actor_messages = Vec::new();
        for message in outcome.messages {
            ensure!(!message.actor.is_empty(), "key-policy actor name is empty");
            if message.actor == manifold_wasm::HOST_ACTOR_NAME {
                host_messages.push(
                    serde_json::from_slice(&message.payload)
                        .context("decode client key-policy host message")?,
                );
            } else {
                actor_messages.push(message);
            }
        }

        let was_passthrough = self.shortcut_passthrough;
        let mut changed_passthrough = false;
        host_messages.retain(|message| match message {
            HostMessage::ToggleShortcutPassthrough => {
                self.shortcut_passthrough = !self.shortcut_passthrough;
                changed_passthrough = true;
                false
            }
            HostMessage::SetShortcutPassthrough { enabled } => {
                self.shortcut_passthrough = *enabled;
                changed_passthrough = true;
                false
            }
            _ => true,
        });

        if was_passthrough && !changed_passthrough {
            host_messages.clear();
            actor_messages.clear();
            return Ok(KeyPolicyEvaluation {
                decision: GlobalKeyDecision::Forward,
                host_messages,
                actor_messages,
            });
        }

        Ok(KeyPolicyEvaluation {
            decision: outcome.decision,
            host_messages,
            actor_messages,
        })
    }
}

fn load_wasm_key_policy(spec: &[String]) -> Result<WasmGlobalKeyPolicy> {
    ensure!(
        spec.len() == 2,
        "key policy requires a provider name and WASM path"
    );
    let provider = spec.first().context("key policy provider is missing")?;
    let path = spec.get(1).context("key policy path is missing")?;
    let policy = WasmGlobalKeyPolicy::load(Path::new(path), provider)
        .with_context(|| format!("load presentation key policy {provider:?} from {path}"))?;
    eprintln!("loaded presentation key policy provider={provider} path={path}");
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_uses_the_client_owned_pane_and_app_inventory() {
        let snapshot = KeyPolicySnapshot {
            focused_pane: Some(u64::MAX),
            panes: vec![u64::MAX, 7],
            managed_panes: vec![7],
            app_panes: vec![KeyPolicyAppPane {
                pane: 7,
                app_id: "firefox".to_string(),
            }],
        };
        let encoded = serde_json::to_value(snapshot);
        assert!(encoded.is_ok());
        if let Ok(encoded) = encoded {
            assert_eq!(
                encoded,
                serde_json::json!({
                    "focused_pane": u64::MAX,
                    "panes": [u64::MAX, 7],
                    "managed_panes": [7],
                    "app_panes": [{"pane": 7, "app_id": "firefox"}],
                })
            );
        }
    }
}
