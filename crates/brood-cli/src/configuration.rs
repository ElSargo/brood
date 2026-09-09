//! Backend configuration data, shared with offline schema generation.
//! Runtime application and semantic validation remain owned by broodd.
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct BroodConfig {
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub module: BTreeMap<String, ModuleConfig>,
    #[serde(default)]
    pub startup: BTreeMap<String, StartupAppConfig>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct SessionConfig {
    pub runner: Option<SessionRunnerConfig>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub struct SessionRunnerConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub kind: String,
    #[serde(default)]
    pub command: Vec<String>,
    pub cache_root: Option<PathBuf>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ModuleConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub kind: String,
    pub path: PathBuf,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, rename = "startup-message")]
    pub startup_message: Option<String>,
    #[serde(default)]
    pub lifecycle: Option<ModuleLifecycleConfig>,
    #[serde(default)]
    pub resources: ModuleResourceConfig,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ModuleResourceConfig {
    #[serde(rename = "filesystem-watches")]
    pub filesystem_watches: Option<usize>,
    #[serde(rename = "filesystem-watch-events")]
    pub filesystem_watch_events: Option<usize>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct ModuleLifecycleConfig {
    #[serde(default = "default_lifecycle_timeout_ms", rename = "start-timeout-ms")]
    pub start_timeout_ms: u64,
    #[serde(default = "default_lifecycle_timeout_ms", rename = "stop-timeout-ms")]
    pub stop_timeout_ms: u64,
    #[serde(default)]
    pub data: BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct StartupAppConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub realm: StartupRealm,
    pub argv: Vec<String>,
}

#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum StartupRealm {
    #[default]
    Session,
}

const fn default_enabled() -> bool {
    true
}

const fn default_lifecycle_timeout_ms() -> u64 {
    1_000
}

#[cfg(test)]
#[allow(
    clippy::panic_in_result_fn,
    reason = "test assertions report contract mismatches while fixture setup uses Result"
)]
mod tests {
    #[test]
    fn generated_configuration_schemas_follow_parser_defaults_and_names() -> anyhow::Result<()> {
        let source = r#"
[init]
backend = "root.wasm"
session = "session.wasm"
[client.key-policy]
provider = "wasm"
path = "keys.wasm"
[session.runner]
kind = "command"
command = ["sh"]
[module.example]
kind = "wasm"
path = "plugin.wasm"
startup-message = "ready"
[module.example.resources]
filesystem-watches = 4
filesystem-watch-events = 8
[module.example.lifecycle]
start-timeout-ms = 250
[startup.editor]
argv = ["editor"]
"#;
        let parsed: super::BroodConfig = toml::from_str(source)?;
        let module = parsed
            .module
            .get("example")
            .ok_or_else(|| anyhow::anyhow!("module missing"))?;
        assert!(module.enabled);
        assert_eq!(module.startup_message.as_deref(), Some("ready"));
        let lifecycle = module
            .lifecycle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("lifecycle missing"))?;
        assert_eq!(lifecycle.start_timeout_ms, 250);
        assert_eq!(lifecycle.stop_timeout_ms, 1000);
        let document = serde_json::to_value(toml::from_str::<toml::Value>(source)?)?;
        let schemas = crate::introspection::generated_schemas();
        for id in ["brood.runtime-config.v1", "brood.realm-init-config.v1"] {
            let schema = schemas
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("schema missing"))?;
            let validator = jsonschema::validator_for(schema)?;
            assert!(validator.is_valid(&document));
        }
        let invalid = source.replace("path = \"plugin.wasm\"", "path = 42");
        assert!(toml::from_str::<super::BroodConfig>(&invalid).is_err());
        let schema = schemas
            .get("brood.runtime-config.v1")
            .ok_or_else(|| anyhow::anyhow!("schema missing"))?;
        let invalid_document = serde_json::to_value(toml::from_str::<toml::Value>(&invalid)?)?;
        assert!(!jsonschema::validator_for(schema)?.is_valid(&invalid_document));
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("brood.toml");
        std::fs::write(&path, source)?;
        assert_eq!(
            crate::realm_init_actor_path(&path, crate::RealmKind::Backend)?,
            Some(directory.path().join("root.wasm"))
        );
        let policy = crate::client_key_policy_selection(&path)?
            .ok_or_else(|| anyhow::anyhow!("key policy missing"))?;
        assert_eq!(policy.provider, "wasm");
        assert_eq!(policy.path, directory.path().join("keys.wasm"));
        Ok(())
    }
}
