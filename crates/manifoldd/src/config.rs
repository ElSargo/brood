use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{state::Manifold, wasm_key::WasmKeyHandler, wasm_map::WasmMapHook};

#[derive(Debug, serde::Deserialize)]
pub struct ManifoldConfig {
    #[serde(default)]
    session: SessionConfig,
    #[serde(default)]
    module: BTreeMap<String, ModuleConfig>,
    #[serde(default)]
    startup: BTreeMap<String, StartupAppConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct SessionConfig {
    runner: Option<SessionRunnerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct SessionRunnerConfig {
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) command: Vec<String>,
    pub(crate) cache_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRunnerSource {
    Manifest,
    Flake,
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRunnerSelection {
    pub(crate) runner: SessionRunnerConfig,
    pub(crate) project_dir: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) priority: u8,
    pub(crate) source: SessionRunnerSource,
}

enum SessionRunnerManifestState {
    Enabled(SessionRunnerConfig),
    Disabled,
    Absent,
}

const MANIFEST_RUNNER_PRIORITY: u8 = 0;
const DEFAULT_RUNNER_PRIORITY: u8 = 1;

#[derive(Clone, Copy)]
struct SessionRunnerDetector {
    file_name: &'static str,
    priority: Option<u8>,
    source: SessionRunnerSource,
    runner: fn() -> SessionRunnerConfig,
}

impl SessionRunnerDetector {
    fn priority(self) -> u8 {
        self.priority.unwrap_or(DEFAULT_RUNNER_PRIORITY)
    }
}

const BUILTIN_RUNNER_DETECTORS: &[SessionRunnerDetector] = &[SessionRunnerDetector {
    file_name: "flake.nix",
    priority: None,
    source: SessionRunnerSource::Flake,
    runner: nix_develop_runner,
}];

#[derive(Debug, serde::Deserialize)]
struct ModuleConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    kind: String,
    path: PathBuf,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct StartupAppConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    realm: StartupRealm,
    argv: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StartupRealm {
    #[default]
    Session,
    Presentation,
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

pub fn load_startup_apps(state: &mut Manifold, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }

    let config = read_config(config_path)?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    for (id, app) in config.startup {
        if !app.enabled {
            info!(startup_app = %id, "config startup app disabled");
            continue;
        }
        if app.argv.is_empty() {
            warn!(startup_app = %id, "configured startup app has empty argv; skipping app");
            continue;
        }
        let argv = resolve_startup_argv(config_dir, &app.argv);
        match app.realm {
            StartupRealm::Session => state
                .spawn_hosted_app_argv(&argv)
                .with_context(|| format!("spawn config startup app {id}"))?,
            StartupRealm::Presentation => state
                .spawn_presentation_app_argv(&argv)
                .with_context(|| format!("spawn config presentation startup app {id}"))?,
        }
        info!(startup_app = %id, realm = ?app.realm, argv = ?argv, "spawned config startup app");
    }

    Ok(())
}

pub(crate) fn discover_session_runner(start_dir: &Path) -> Result<Option<SessionRunnerSelection>> {
    let start_dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());

    if let Some(config_path) = find_upwards(&start_dir, "manifold.toml") {
        match load_session_runner_manifest(&config_path)? {
            SessionRunnerManifestState::Enabled(runner) => {
                let project_dir = config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                return Ok(Some(SessionRunnerSelection {
                    runner,
                    project_dir,
                    config_path,
                    priority: MANIFEST_RUNNER_PRIORITY,
                    source: SessionRunnerSource::Manifest,
                }));
            }
            SessionRunnerManifestState::Disabled => return Ok(None),
            SessionRunnerManifestState::Absent => {}
        }
    }

    for detector in BUILTIN_RUNNER_DETECTORS {
        if let Some(marker_path) = find_upwards(&start_dir, detector.file_name) {
            let project_dir = marker_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            return Ok(Some(SessionRunnerSelection {
                runner: (detector.runner)(),
                config_path: project_dir.join("manifold.toml"),
                project_dir,
                priority: detector.priority(),
                source: detector.source,
            }));
        }
    }

    Ok(None)
}

pub(crate) fn global_config_path() -> PathBuf {
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

pub(crate) fn override_session_runner(
    kind: String,
    project_dir: PathBuf,
    config_path: PathBuf,
) -> SessionRunnerSelection {
    SessionRunnerSelection {
        runner: SessionRunnerConfig {
            enabled: true,
            kind,
            command: Vec::new(),
            cache_root: None,
        },
        project_dir,
        config_path,
        priority: 0,
        source: SessionRunnerSource::Override,
    }
}

fn load_session_runner_manifest(config_path: &Path) -> Result<SessionRunnerManifestState> {
    if !config_path.exists() {
        return Ok(SessionRunnerManifestState::Absent);
    }

    let Some(mut runner) = read_config(config_path)?.session.runner else {
        return Ok(SessionRunnerManifestState::Absent);
    };
    if !runner.enabled {
        return Ok(SessionRunnerManifestState::Disabled);
    }
    if let Some(cache_root) = runner.cache_root.as_deref() {
        runner.cache_root = Some(resolve_path(
            config_path.parent().unwrap_or_else(|| Path::new(".")),
            cache_root,
        ));
    }
    Ok(SessionRunnerManifestState::Enabled(runner))
}

fn nix_develop_runner() -> SessionRunnerConfig {
    SessionRunnerConfig {
        enabled: true,
        kind: "nix-develop".to_string(),
        command: Vec::new(),
        cache_root: None,
    }
}

fn find_upwards(start_dir: &Path, file_name: &str) -> Option<PathBuf> {
    for dir in start_dir.ancestors() {
        let candidate = dir.join(file_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
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

fn resolve_startup_argv(config_dir: &Path, argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|arg| resolve_startup_arg(config_dir, arg))
        .collect()
}

fn resolve_startup_arg(config_dir: &Path, arg: &str) -> String {
    let path = Path::new(arg);
    if path.is_absolute() || !is_relative_path_like_arg(path, arg) {
        return arg.to_string();
    }
    let resolved = config_dir.join(path);
    if resolved.exists() {
        resolved.to_string_lossy().into_owned()
    } else {
        arg.to_string()
    }
}

fn is_relative_path_like_arg(path: &Path, arg: &str) -> bool {
    !arg.starts_with('-') && (arg.starts_with('.') || path.components().count() > 1)
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        BUILTIN_RUNNER_DETECTORS, DEFAULT_RUNNER_PRIORITY, SessionRunnerSource,
        discover_session_runner, read_config, repo_config_path_from_current_exe, resolve_path,
        resolve_startup_argv,
    };

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

            [startup.agent]
            realm = "session"
            argv = ["foot", "--app-id", "manifold-codex", "codex", "--yolo"]

            [startup.switcher]
            realm = "presentation"
            argv = ["python3", "apps/manifold-session-switcher.py"]

            [session.runner]
            enabled = true
            kind = "nix-develop"
            cache_root = ".manifold/cache"
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

        let app = config.startup.get("agent").expect("agent startup app");
        assert!(app.enabled);
        assert_eq!(
            app.argv,
            ["foot", "--app-id", "manifold-codex", "codex", "--yolo"]
        );
        assert_eq!(app.realm, super::StartupRealm::Session);

        let app = config
            .startup
            .get("switcher")
            .expect("switcher startup app");
        assert!(app.enabled);
        assert_eq!(app.realm, super::StartupRealm::Presentation);
        assert_eq!(app.argv, ["python3", "apps/manifold-session-switcher.py"]);

        let runner = config.session.runner.expect("session runner");
        assert!(runner.enabled);
        assert_eq!(runner.kind, "nix-develop");
        assert_eq!(runner.cache_root, Some(PathBuf::from(".manifold/cache")));
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
    fn resolves_startup_path_arguments_against_config_dir() {
        let root = temp_dir("startup-argv-paths");
        let apps = root.join("apps");
        std::fs::create_dir_all(&apps).expect("create apps dir");
        std::fs::write(apps.join("manifold-session-switcher.py"), "").expect("write app");

        let argv = resolve_startup_argv(
            &root,
            &[
                "python3".to_string(),
                "apps/manifold-session-switcher.py".to_string(),
                "--width".to_string(),
                "300".to_string(),
            ],
        );

        assert_eq!(argv[0], "python3");
        assert_eq!(
            argv[1],
            root.join("apps")
                .join("manifold-session-switcher.py")
                .to_string_lossy()
        );
        assert_eq!(argv[2], "--width");
        assert_eq!(argv[3], "300");

        std::fs::remove_dir_all(root).expect("remove temp dir");
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

    #[test]
    fn discovers_manifest_runner_from_parent_directory() {
        let root = temp_dir("manifest-runner");
        let child = root.join("src").join("bin");
        std::fs::create_dir_all(&child).expect("create child dir");
        std::fs::write(
            root.join("manifold.toml"),
            r#"
            [session.runner]
            kind = "custom"
            command = ["./runner.sh"]
            cache_root = ".manifold/cache"
            "#,
        )
        .expect("write manifest");

        let selection = discover_session_runner(&child)
            .expect("discover runner")
            .expect("runner selected");
        assert_eq!(selection.source, SessionRunnerSource::Manifest);
        assert_eq!(selection.priority, 0);
        assert_eq!(selection.project_dir, root);
        assert_eq!(
            selection.config_path,
            selection.project_dir.join("manifold.toml")
        );
        assert_eq!(selection.runner.kind, "custom");
        assert_eq!(
            selection.runner.cache_root,
            Some(selection.project_dir.join(".manifold/cache"))
        );

        std::fs::remove_dir_all(selection.project_dir).expect("remove temp dir");
    }

    #[test]
    fn repo_config_fallback_uses_binary_location() {
        let path = repo_config_path_from_current_exe().expect("repo config from test binary");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("manifold.toml")
        );
        assert!(
            path.exists(),
            "repo config should exist at {}",
            path.display()
        );
    }

    #[test]
    fn disabled_manifest_runner_blocks_flake_fallback() {
        let root = temp_dir("disabled-runner");
        let child = root.join("subdir");
        std::fs::create_dir_all(&child).expect("create child dir");
        std::fs::write(root.join("flake.nix"), "{ outputs = _: {}; }").expect("write flake");
        std::fs::write(
            root.join("manifold.toml"),
            r#"
            [session.runner]
            enabled = false
            kind = "nix-develop"
            "#,
        )
        .expect("write manifest");

        let selection = discover_session_runner(&child).expect("discover runner");
        assert!(selection.is_none());

        std::fs::remove_dir_all(root).expect("remove temp dir");
    }

    #[test]
    fn discovers_nix_runner_from_parent_flake() {
        let root = temp_dir("flake-runner");
        let child = root.join("subdir");
        std::fs::create_dir_all(&child).expect("create child dir");
        std::fs::write(root.join("flake.nix"), "{ outputs = _: {}; }").expect("write flake");

        let selection = discover_session_runner(&child)
            .expect("discover runner")
            .expect("runner selected");
        assert_eq!(selection.source, SessionRunnerSource::Flake);
        assert_eq!(selection.priority, 1);
        assert_eq!(selection.project_dir, root);
        assert_eq!(
            selection.config_path,
            selection.project_dir.join("manifold.toml")
        );
        assert_eq!(selection.runner.kind, "nix-develop");

        std::fs::remove_dir_all(selection.project_dir).expect("remove temp dir");
    }

    #[test]
    fn detector_without_priority_defaults_to_one() {
        let detector = BUILTIN_RUNNER_DETECTORS
            .iter()
            .find(|detector| detector.file_name == "flake.nix")
            .expect("flake detector");
        assert_eq!(detector.priority, None);
        assert_eq!(detector.priority(), DEFAULT_RUNNER_PRIORITY);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "manifold-config-test-{name}-{}-{stamp}",
            std::process::id()
        ))
    }
}
