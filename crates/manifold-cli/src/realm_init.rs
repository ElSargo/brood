use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const MANIFOLD_CONFIG_ENV: &str = "MANIFOLD_CONFIG";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealmKind {
    Session,
    Client,
}

#[derive(Default, serde::Deserialize)]
struct RootConfig {
    #[serde(default)]
    init: InitConfig,
}

#[derive(Default, serde::Deserialize)]
struct InitConfig {
    session: Option<PathBuf>,
    client: Option<PathBuf>,
}

/// Resolve the blessed init actor for one realm from a Manifold config.
pub fn realm_init_actor_path(config_path: &Path, realm: RealmKind) -> Result<Option<PathBuf>> {
    if !config_path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(config_path)
        .with_context(|| format!("read Manifold config {}", config_path.display()))?;
    let config: RootConfig = toml::from_str(&contents)
        .with_context(|| format!("parse Manifold config {}", config_path.display()))?;
    let path = match realm {
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

/// Return the shared config path used by both session and client realms.
#[must_use]
pub fn global_config_path() -> PathBuf {
    if let Some(config_path) = env::var_os(MANIFOLD_CONFIG_ENV).filter(|path| !path.is_empty()) {
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
    config_home.join("manifold").join("manifold.toml")
}

fn repo_config_path_from_current_exe() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let start_dir = exe.parent()?;
    find_upwards(start_dir, "manifold.toml")
}

fn find_upwards(start_dir: &Path, file_name: &str) -> Option<PathBuf> {
    start_dir.ancestors().find_map(|dir| {
        let candidate = dir.join(file_name);
        candidate.exists().then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_paths_resolve_relative_to_config() {
        let root = tempfile::tempdir();
        assert!(root.is_ok());
        let Some(root) = root.ok() else {
            return;
        };
        let config_path = root.path().join("manifold.toml");
        let write = fs::write(
            &config_path,
            "[init]\nsession = \"actors/session.wasm\"\nclient = \"actors/client.wasm\"\n",
        );
        assert!(write.is_ok());
        assert_eq!(
            realm_init_actor_path(&config_path, RealmKind::Session)
                .ok()
                .flatten(),
            Some(root.path().join("actors/session.wasm"))
        );
        assert_eq!(
            realm_init_actor_path(&config_path, RealmKind::Client)
                .ok()
                .flatten(),
            Some(root.path().join("actors/client.wasm"))
        );
    }
}
