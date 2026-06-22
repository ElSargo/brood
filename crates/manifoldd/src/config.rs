use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{state::Manifold, wasm_key::WasmKeyHandler, wasm_map::WasmMapHook};

#[derive(Debug, serde::Deserialize)]
pub struct ManifoldConfig {
    #[serde(default)]
    module: BTreeMap<String, ModuleConfig>,
}

#[derive(Debug, serde::Deserialize)]
struct ModuleConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    kind: String,
    path: PathBuf,
    #[serde(default)]
    capabilities: Vec<String>,
}

pub fn load_startup_modules(state: &mut Manifold, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        info!(
            path = %config_path.display(),
            "no Manifold config found; starting without config modules"
        );
        return Ok(());
    }

    let config = read_config(config_path)?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    for (id, module) in config.module {
        if !module.enabled {
            info!(module = %id, "config module disabled");
            continue;
        }

        match module.kind.as_str() {
            "fast-key-handler" => {
                let module_path = resolve_path(config_dir, &module.path);
                if !module_path.exists() {
                    warn!(
                        module = %id,
                        path = %module_path.display(),
                        "configured WASM key handler does not exist; skipping module"
                    );
                    continue;
                }
                state.wasm_key_handler = Some(
                    WasmKeyHandler::load(&module_path)
                        .with_context(|| format!("load config module {id}"))?,
                );
                info!(
                    module = %id,
                    path = %module_path.display(),
                    capabilities = ?module.capabilities,
                    "loaded config WASM key handler"
                );
            }
            "fast-map-hook" => {
                let module_path = resolve_path(config_dir, &module.path);
                if !module_path.exists() {
                    warn!(
                        module = %id,
                        path = %module_path.display(),
                        "configured WASM map hook does not exist; skipping module"
                    );
                    continue;
                }
                state.wasm_map_hook = Some(
                    WasmMapHook::load(&module_path)
                        .with_context(|| format!("load config module {id}"))?,
                );
                info!(
                    module = %id,
                    path = %module_path.display(),
                    capabilities = ?module.capabilities,
                    "loaded config WASM map hook"
                );
            }
            unsupported => {
                warn!(
                    module = %id,
                    kind = %unsupported,
                    "unsupported config module kind; skipping module"
                );
            }
        }
    }

    Ok(())
}

fn read_config(path: &Path) -> Result<ManifoldConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read Manifold config {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parse Manifold config {}", path.display()))
}

fn resolve_path(config_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{read_config, resolve_path};

    #[test]
    fn parses_module_table() {
        let config: super::ManifoldConfig = toml::from_str(
            r#"
            [module.dev]
            enabled = true
            kind = "fast-key-handler"
            path = "target/dev.wasm"
            capabilities = ["apps:run-argv"]

            [module.layout]
            kind = "fast-map-hook"
            path = "target/layout.wasm"
            capabilities = ["layout:on-map"]
            "#,
        )
        .expect("parse config");

        let module = config.module.get("dev").expect("dev module");
        assert!(module.enabled);
        assert_eq!(module.kind, "fast-key-handler");
        assert_eq!(module.path, PathBuf::from("target/dev.wasm"));
        assert_eq!(module.capabilities, ["apps:run-argv"]);

        let module = config.module.get("layout").expect("layout module");
        assert!(module.enabled);
        assert_eq!(module.kind, "fast-map-hook");
        assert_eq!(module.path, PathBuf::from("target/layout.wasm"));
        assert_eq!(module.capabilities, ["layout:on-map"]);
    }

    #[test]
    fn resolves_relative_paths_against_config_dir() {
        assert_eq!(
            resolve_path(Path::new("/tmp/project"), Path::new("module.wasm")),
            PathBuf::from("/tmp/project/module.wasm")
        );
        assert_eq!(
            resolve_path(Path::new("/tmp/project"), Path::new("/abs/module.wasm")),
            PathBuf::from("/abs/module.wasm")
        );
    }

    #[test]
    fn malformed_config_reports_error() {
        let dir = std::env::temp_dir().join(format!("manifold-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("manifold.toml");
        std::fs::write(&path, "not = [valid").expect("write config");

        let err = read_config(&path).expect_err("malformed config should fail");
        assert!(err.to_string().contains("parse Manifold config"));
        std::fs::remove_file(path).expect("remove temp config");
        std::fs::remove_dir(dir).expect("remove temp dir");
    }
}
