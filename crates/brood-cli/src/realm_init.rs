use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const BROOD_CONFIG_ENV: &str = "BROOD_CONFIG";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealmKind {
    Backend,
    Session,
    Client,
}

#[derive(Default, serde::Deserialize)]
struct RootConfig {
    #[serde(default)]
    init: InitConfig,
    #[serde(default)]
    client: ClientConfig,
}

#[derive(Default, serde::Deserialize)]
struct InitConfig {
    backend: Option<PathBuf>,
    session: Option<PathBuf>,
    client: Option<PathBuf>,
}

#[derive(Default, serde::Deserialize)]
struct ClientConfig {
    #[serde(rename = "key-policy")]
    key_policy: Option<ClientKeyPolicyConfig>,
}

#[derive(serde::Deserialize)]
struct ClientKeyPolicyConfig {
    provider: String,
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientKeyPolicySelection {
    pub provider: String,
    pub path: PathBuf,
}

/// Resolve the blessed init actor for one realm from a Brood config.
pub fn realm_init_actor_path(config_path: &Path, realm: RealmKind) -> Result<Option<PathBuf>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(config_path)
        .with_context(|| format!("read Brood config {}", config_path.display()))?;
    let config: RootConfig = toml::from_str(&contents)
        .with_context(|| format!("parse Brood config {}", config_path.display()))?;
    let path = match realm {
        RealmKind::Backend => config.init.backend,
        RealmKind::Session => config.init.session,
        RealmKind::Client => config.init.client,
    };
    Ok(path.map(|path| {
        if path.is_absolute() {
            path
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        }
    }))
}

/// Resolve the synchronous key policy selected for the presentation client.
pub fn client_key_policy_selection(config_path: &Path) -> Result<Option<ClientKeyPolicySelection>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(config_path)
        .with_context(|| format!("read Brood config {}", config_path.display()))?;
    let config: RootConfig = toml::from_str(&contents)
        .with_context(|| format!("parse Brood config {}", config_path.display()))?;
    Ok(config
        .client
        .key_policy
        .map(|policy| ClientKeyPolicySelection {
            provider: policy.provider,
            path: resolve_relative_path(config_path, policy.path),
        }))
}

fn resolve_relative_path(config_path: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

/// Return the shared config path used by both session and client realms.
#[must_use]
pub fn global_config_path() -> PathBuf {
    if let Some(config_path) = env::var_os(BROOD_CONFIG_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(config_path);
    }
    let config_path = xdg_config_path();
    if config_path.exists() {
        return config_path;
    }
    repo_config_path_from_current_exe().unwrap_or(config_path)
}

fn xdg_config_path() -> PathBuf {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(env::temp_dir);
    config_home.join("brood").join("brood.toml")
}

/// Return the persistent presentation-global keyboard override path.
#[must_use]
pub fn global_keybindings_path() -> PathBuf {
    if let Some(path) = env::var_os("BROOD_KEYBINDINGS_PATH").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    xdg_config_path().with_file_name("keybindings.json")
}

fn repo_config_path_from_current_exe() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let start_dir = exe.parent()?;
    find_upwards(start_dir, "brood.toml")
}

fn find_upwards(start_dir: &Path, file_name: &str) -> Option<PathBuf> {
    start_dir.ancestors().find_map(|dir| {
        let candidate = dir.join(file_name);
        candidate.exists().then_some(candidate)
    })
}
