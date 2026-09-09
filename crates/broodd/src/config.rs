use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use tracing::{info, warn};

use crate::state::Brood;
pub use brood_cli::configuration::SessionRunnerConfig;
use brood_cli::configuration::{BroodConfig, ModuleResourceConfig, StartupRealm};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRunnerSource {
    Manifest,
    Flake,
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRunnerSelection {
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

fn configured_plugin_resources(
    resources: &ModuleResourceConfig,
) -> Result<crate::state::ConfiguredPluginResources> {
    let defaults = crate::state::ConfiguredPluginResources::default();
    let filesystem_watches = resources
        .filesystem_watches
        .unwrap_or(defaults.filesystem_watches);
    let filesystem_watch_events = resources
        .filesystem_watch_events
        .unwrap_or(defaults.filesystem_watch_events);
    ensure!(
        filesystem_watches > 0 && filesystem_watch_events > 0,
        "plugin filesystem watch resource limits must be non-zero"
    );
    ensure!(
        filesystem_watches <= defaults.filesystem_watches
            && filesystem_watch_events <= defaults.filesystem_watch_events,
        "plugin filesystem watch resource limits cannot exceed daemon ceilings"
    );
    Ok(crate::state::ConfiguredPluginResources {
        filesystem_watches,
        filesystem_watch_events,
    })
}

pub fn load_runtime_settings(_state: &mut Brood, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        return Ok(());
    }

    read_config(config_path)?;
    Ok(())
}

pub fn load_startup_modules(state: &mut Brood, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        info!(
            path = %config_path.display(),
            "no Brood config found; starting without config modules"
        );
        return Ok(());
    }

    let config = read_config(config_path)?;

    for (id, module) in config.module {
        if !module.enabled {
            info!(module = %id, "config module disabled");
            continue;
        }

        match module.kind.as_str() {
            "wasm-plugin" | "plugin" => {
                ensure!(
                    module.startup_message.is_none() || module.lifecycle.is_none(),
                    "module {id} cannot combine legacy startup-message with lifecycle"
                );
                if let Some(lifecycle) = &module.lifecycle {
                    ensure!(
                        lifecycle.start_timeout_ms > 0 && lifecycle.stop_timeout_ms > 0,
                        "module {id} lifecycle timeouts must be non-zero"
                    );
                    ensure!(
                        lifecycle.data.keys().all(|key| !key.trim().is_empty()),
                        "module {id} lifecycle data keys must not be empty"
                    );
                }
                let module_path = resolve_path(
                    config_path.parent().unwrap_or_else(|| Path::new(".")),
                    &module.path,
                );
                if !module_path.exists() {
                    warn!(
                        module = %id,
                        path = %module_path.display(),
                        "configured WASM plugin does not exist; skipping module"
                    );
                    continue;
                }
                let workspace_filesystem = workspace_filesystem_capability(&module.capabilities);
                let runtime_io =
                    runtime_io_capabilities(&module.capabilities).with_context(|| {
                        format!("validate config module {id} runtime I/O capabilities")
                    })?;
                state
                    .replace_configured_wasm_plugin(
                        &id,
                        &module_path,
                        workspace_filesystem,
                        module.startup_message,
                        module
                            .lifecycle
                            .map(|lifecycle| crate::state::ConfiguredPluginLifecycle {
                                start_timeout: Duration::from_millis(lifecycle.start_timeout_ms),
                                stop_timeout: Duration::from_millis(lifecycle.stop_timeout_ms),
                                data: lifecycle.data,
                            }),
                        configured_plugin_resources(&module.resources)
                            .with_context(|| format!("validate config module {id} resources"))?,
                        runtime_io,
                    )
                    .with_context(|| format!("load config module {id}"))?;
                info!(
                    module = %id,
                    path = %module_path.display(),
                    capabilities = ?module.capabilities,
                    "loaded config WASM plugin"
                );
            }
            "fast-key-handler" | "fast-map-hook" => {
                warn!(
                    module = %id,
                    kind = %module.kind,
                    "legacy WASM module kind is no longer supported; skipping module"
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

pub fn workspace_filesystem_capability(capabilities: &[String]) -> Option<bool> {
    if capabilities
        .iter()
        .any(|capability| capability == "filesystem:workspace-write")
    {
        Some(true)
    } else if capabilities
        .iter()
        .any(|capability| capability == "filesystem:workspace-read")
    {
        Some(false)
    } else {
        None
    }
}

pub fn runtime_io_capabilities(
    capabilities: &[String],
) -> Result<brood_wasm::RuntimeIoCapabilities> {
    let mut commands = capabilities
        .iter()
        .filter_map(|capability| capability.strip_prefix("subprocess:"))
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    ensure!(
        commands.iter().all(|command| {
            let mut components = Path::new(command).components();
            !command.is_empty()
                && matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none()
        }),
        "subprocess capability must name one PATH command"
    );
    commands.sort();
    commands.dedup();
    Ok(brood_wasm::RuntimeIoCapabilities {
        commands,
        allow_pty: capabilities.iter().any(|capability| capability == "pty"),
        allow_loopback: capabilities
            .iter()
            .any(|capability| capability == "network:loopback"),
    })
}

pub fn load_startup_apps(state: &mut Brood, config_path: &Path) -> Result<()> {
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
                .with_context(|| format!("spawn config startup app {id}"))
                .map(|_| ())?,
        }
        info!(startup_app = %id, realm = ?app.realm, argv = ?argv, "spawned config startup app");
    }

    Ok(())
}

pub fn discover_session_runner(start_dir: &Path) -> Result<Option<SessionRunnerSelection>> {
    let start_dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());

    if let Some(config_path) = find_upwards(&start_dir, "brood.toml") {
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
                config_path: project_dir.join("brood.toml"),
                project_dir,
                priority: detector.priority(),
                source: detector.source,
            }));
        }
    }

    Ok(None)
}

pub fn global_config_path() -> PathBuf {
    brood_cli::global_config_path()
}

pub const fn override_session_runner(
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

fn read_config(path: &Path) -> Result<BroodConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read Brood config {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parse Brood config {}", path.display()))
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
