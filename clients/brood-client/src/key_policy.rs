use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use brood_keybindings::{ACTIONS, Binding, action};
use brood_wasm::{
    ABI_INPUT_MODE_PASSTHROUGH, GlobalKeyActorMessage, GlobalKeyDecision, GlobalKeyPolicyInput,
    GlobalKeyPolicyInvocation, HostMessage, Registry, WasmGlobalKeyPolicy,
};
use serde::{Deserialize, Serialize};

pub struct ClientKeyPolicy {
    wasm: Option<WasmGlobalKeyPolicy>,
    publications: Registry,
    publication_actor: brood_wasm::ActorId,
    _publication_mailbox: brood_wasm::Mailbox,
    shortcut_passthrough: bool,
    binding_overrides: BTreeMap<String, Binding>,
    bindings_path: PathBuf,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeyBindingOverride {
    pub action: String,
    pub key_code: i32,
    pub modifiers: i32,
}

#[derive(Serialize)]
struct KeyPolicyPublication<'a> {
    focused_pane: Option<u64>,
    panes: &'a [u64],
    managed_panes: &'a [u64],
    app_panes: &'a [KeyPolicyAppPane],
    bindings: Vec<KeyBindingOverride>,
}

#[derive(Deserialize, Serialize)]
struct StoredBindings {
    version: u32,
    bindings: Vec<KeyBindingOverride>,
}

#[derive(Debug)]
pub struct KeyPolicyEvaluation {
    pub decision: GlobalKeyDecision,
    pub host_messages: Vec<HostMessage>,
    pub actor_messages: Vec<GlobalKeyActorMessage>,
}

impl ClientKeyPolicy {
    pub fn load(spec: Option<&[String]>, bindings_path: PathBuf) -> Result<Self> {
        let wasm = spec.map(load_wasm_key_policy).transpose()?;
        let mut publications = Registry::new();
        let publication_mailbox = publications
            .register_actor("presentation-client-state", 1)
            .context("register presentation-client policy publication source")?;
        let publication_actor = publication_mailbox.actor_id();
        let binding_overrides = load_binding_overrides(&bindings_path);
        Ok(Self {
            wasm,
            publications,
            publication_actor,
            _publication_mailbox: publication_mailbox,
            shortcut_passthrough: false,
            binding_overrides,
            bindings_path,
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
        if self.wasm.is_none() {
            return Ok(KeyPolicyEvaluation {
                decision: GlobalKeyDecision::Abstain,
                host_messages: Vec::new(),
                actor_messages: Vec::new(),
            });
        }

        let publication_state = KeyPolicyPublication {
            focused_pane: snapshot.focused_pane,
            panes: &snapshot.panes,
            managed_panes: &snapshot.managed_panes,
            app_panes: &snapshot.app_panes,
            bindings: self.binding_overrides(),
        };
        let publication = self.publications.append_default_publication_frame(
            self.publication_actor,
            serde_json::to_vec(&publication_state)
                .context("encode client key-policy publication")?,
        )?;
        let input = GlobalKeyPolicyInput::new(
            key_code,
            key_state,
            modifiers,
            ABI_INPUT_MODE_PASSTHROUGH,
            repeat,
        );
        let policy = self
            .wasm
            .as_mut()
            .context("WASM key policy disappeared during evaluation")?;
        let outcome =
            policy.handle_key(GlobalKeyPolicyInvocation::new(input, Some(&publication)))?;
        let mut host_messages = Vec::new();
        let mut actor_messages = Vec::new();
        for message in outcome.messages {
            ensure!(!message.actor.is_empty(), "key-policy actor name is empty");
            if message.actor == brood_wasm::HOST_ACTOR_NAME {
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

    #[must_use]
    pub fn binding_overrides(&self) -> Vec<KeyBindingOverride> {
        self.binding_overrides
            .iter()
            .map(|(action, binding)| KeyBindingOverride {
                action: action.clone(),
                key_code: binding.key_code,
                modifiers: binding.modifiers,
            })
            .collect()
    }

    pub fn set_binding(&mut self, action_id: &str, binding: Binding) -> Result<()> {
        ensure!(binding.is_valid(), "key binding is invalid");
        let target = action(action_id)
            .with_context(|| format!("key binding action {action_id:?} is unknown"))?;
        let old_binding = self.effective_binding(target.id);
        if old_binding == binding {
            return Ok(());
        }
        let conflict = ACTIONS
            .iter()
            .find(|candidate| {
                candidate.id != target.id && self.effective_binding(candidate.id) == binding
            })
            .map(|candidate| candidate.id);

        self.set_override(target.id, binding);
        if let Some(conflict) = conflict {
            self.set_override(conflict, old_binding);
        }
        self.persist_bindings()
    }

    fn effective_binding(&self, action_id: &str) -> Binding {
        self.binding_overrides
            .get(action_id)
            .copied()
            .or_else(|| action(action_id).map(|action| action.default_binding))
            .unwrap_or(Binding::new(0, 0))
    }

    fn set_override(&mut self, action_id: &str, binding: Binding) {
        let Some(action) = action(action_id) else {
            return;
        };
        if binding == action.default_binding {
            self.binding_overrides.remove(action_id);
        } else {
            self.binding_overrides
                .insert(action_id.to_string(), binding);
        }
    }

    fn persist_bindings(&self) -> Result<()> {
        let parent = self
            .bindings_path
            .parent()
            .context("keybindings path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create keybindings directory {}", parent.display()))?;
        let document = StoredBindings {
            version: 1,
            bindings: self.binding_overrides(),
        };
        let bytes = serde_json::to_vec_pretty(&document).context("encode global keybindings")?;
        let temporary = self
            .bindings_path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        fs::write(&temporary, bytes)
            .with_context(|| format!("write temporary keybindings {}", temporary.display()))?;
        fs::rename(&temporary, &self.bindings_path).with_context(|| {
            format!(
                "replace global keybindings {}",
                self.bindings_path.display()
            )
        })
    }
}

fn load_binding_overrides(path: &Path) -> BTreeMap<String, Binding> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            eprintln!(
                "could not read global keybindings {}: {error}",
                path.display()
            );
            return BTreeMap::new();
        }
    };
    let stored = match serde_json::from_slice::<StoredBindings>(&bytes) {
        Ok(stored) if stored.version == 1 => stored,
        Ok(stored) => {
            eprintln!(
                "ignored unsupported global keybindings version {} in {}",
                stored.version,
                path.display()
            );
            return BTreeMap::new();
        }
        Err(error) => {
            eprintln!(
                "ignored malformed global keybindings {}: {error}",
                path.display()
            );
            return BTreeMap::new();
        }
    };
    let mut overrides = BTreeMap::new();
    for candidate in stored.bindings {
        let binding = Binding::new(candidate.key_code, candidate.modifiers);
        let Some(action) = action(&candidate.action) else {
            eprintln!(
                "ignored unknown keybinding action {:?} in {}",
                candidate.action,
                path.display()
            );
            continue;
        };
        if !binding.is_valid() {
            eprintln!(
                "ignored invalid binding for action {:?} in {}",
                candidate.action,
                path.display()
            );
            continue;
        }
        if binding != action.default_binding {
            overrides.insert(candidate.action, binding);
        }
    }
    overrides
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
    fn remapping_swaps_conflicts_and_survives_reload() -> Result<()> {
        let directory = tempfile::tempdir().context("temporary keybinding directory")?;
        let path = directory.path().join("keybindings.json");
        let mut policy =
            ClientKeyPolicy::load(None, path.clone()).context("load empty keybinding state")?;
        let open = action("keybindings.open").context("open action")?;
        let passthrough = action("shortcuts.passthrough").context("passthrough action")?;

        policy
            .set_binding(open.id, passthrough.default_binding)
            .context("save swapped keybinding")?;
        assert_eq!(
            policy.effective_binding(open.id),
            passthrough.default_binding
        );
        assert_eq!(
            policy.effective_binding(passthrough.id),
            open.default_binding
        );

        let reloaded = ClientKeyPolicy::load(None, path).context("reload keybinding state")?;
        assert_eq!(
            reloaded.effective_binding(open.id),
            passthrough.default_binding
        );
        assert_eq!(
            reloaded.effective_binding(passthrough.id),
            open.default_binding
        );
        Ok(())
    }
}
