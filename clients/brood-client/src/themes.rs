//! Host-owned theme discovery, previews, persistence, and filesystem reloads.
use crate::ClientEvent;
use anyhow::{Context, Result, bail};
use brood_graphics::theme::ThemeSnapshot;
use brood_theme::ThemeLoader;
use notify::Watcher;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
};
use winit::event_loop::EventLoopProxy;

pub struct ThemeCommand {
    pub request: u64,
    pub operation: String,
    pub name: Option<String>,
}

enum Work {
    Reload,
    Request(ThemeCommand),
}

pub struct Controller(mpsc::Sender<Work>);

impl Controller {
    pub fn request(&self, request: ThemeCommand) -> Result<()> {
        self.0
            .send(Work::Request(request))
            .context("theme loader stopped")
    }
}

struct Selection {
    config: PathBuf,
    loader: ThemeLoader,
    committed: ThemeSnapshot,
    active: ThemeSnapshot,
    preview: Option<String>,
}

fn load(config: &Path, loader: &ThemeLoader) -> Result<ThemeSnapshot> {
    let contents = match std::fs::read_to_string(config) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("read theme selection config"),
    };
    let table: toml::Table = toml::from_str(&contents).context("parse theme selection config")?;
    let name = table.get("theme").map_or(Ok("ashen"), |value| {
        value.as_str().context("theme must be a Helix theme name")
    })?;
    loader.load(name, 1)
}

impl Selection {
    fn new(config: PathBuf) -> Result<Self> {
        let loader = ThemeLoader {
            directories: vec![config.with_file_name("themes")],
        };
        let fallback = ThemeLoader::default().load("ashen", 1)?;
        let committed = load(&config, &loader).unwrap_or_else(|error| {
            eprintln!("THEME_REJECTED startup: {error:#}");
            // No user overrides are involved in the startup fallback.
            fallback
        });
        Ok(Self {
            config,
            loader,
            active: committed.clone(),
            committed,
            preview: None,
        })
    }

    fn activate(&mut self, mut next: ThemeSnapshot) -> Result<Option<ThemeSnapshot>> {
        if next.name == self.active.name && next.roles == self.active.roles {
            return Ok(None);
        }
        next.revision = self
            .active
            .revision
            .checked_add(1)
            .context("theme revision exhausted")?;
        self.active = next.clone();
        Ok(Some(next))
    }

    fn reload(&mut self) -> Result<Option<ThemeSnapshot>> {
        self.committed = load(&self.config, &self.loader)?;
        let next = if let Some(name) = &self.preview {
            self.loader.load(name, 1)?
        } else {
            self.committed.clone()
        };
        self.activate(next)
    }

    fn apply(
        &mut self,
        request: &ThemeCommand,
    ) -> Result<(Option<ThemeSnapshot>, serde_json::Value)> {
        let next = match request.operation.as_str() {
            "list" => {
                return Ok((
                    None,
                    serde_json::json!({
                        "names": self.loader.names()?, "current": self.committed.name,
                    }),
                ));
            }
            "preview" => {
                let next = request.name.as_ref().map_or_else(
                    || Ok(self.committed.clone()),
                    |name| self.loader.load(name, 1),
                )?;
                self.preview.clone_from(&request.name);
                next
            }
            "cancel" => {
                self.preview = None;
                self.committed.clone()
            }
            "commit" => {
                let name = request
                    .name
                    .as_deref()
                    .context("theme selection needs a name")?;
                let next = self.loader.load(name, 1)?;
                save(&self.config, name)?;
                self.committed = next.clone();
                self.preview = None;
                next
            }
            other => bail!("unknown theme operation {other:?}"),
        };
        Ok((self.activate(next)?, serde_json::json!({})))
    }
}

// Read at commit time so edits made while the picker was open are preserved.
// Keep comments and formatting; publish the complete file with an atomic rename.
fn save(config: &Path, name: &str) -> Result<()> {
    let config = if config.is_symlink() {
        std::fs::canonicalize(config)?
    } else {
        config.to_path_buf()
    };
    let contents = match std::fs::read_to_string(&config) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("read theme configuration before saving"),
    };
    let mut document: toml_edit::DocumentMut = contents
        .parse()
        .context("parse theme config before saving")?;
    document.insert("theme", toml_edit::value(name));
    let directory = config
        .parent()
        .context("theme config needs a parent directory")?;
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    if let Ok(metadata) = std::fs::metadata(&config) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    temporary.write_all(document.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&config)
        .context("save theme configuration")?;
    Ok(())
}

/// All file and picker work shares one worker and one monotonic revision source.
/// A filesystem event cannot overwrite a preview with an older saved revision.
pub fn watch(
    config: PathBuf,
    proxy: EventLoopProxy<ClientEvent>,
) -> Result<(ThemeSnapshot, Controller)> {
    let config = std::path::absolute(config)?;
    let mut selection = Selection::new(config.clone())?;
    let initial = selection.active.clone();
    let (sender, receiver) = mpsc::channel();
    let changes = sender.clone();
    std::thread::Builder::new()
        .name("brood-theme-loader".into())
        .spawn(move || {
            let theme_directory = config.with_file_name("themes");
            let watched_config = config.clone();
            let watched_themes = theme_directory.clone();
            let result = (|| -> Result<()> {
                let mut watcher =
                    notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                        if event.as_ref().is_ok_and(|event| {
                            !matches!(event.kind, notify::EventKind::Access(_))
                                && event.paths.iter().any(|path| {
                                    path == &watched_config || path.starts_with(&watched_themes)
                                })
                        }) {
                            let _queued = changes.send(Work::Reload);
                        }
                    })?;
                let directory = config
                    .parent()
                    .context("theme config needs a parent directory")?;
                std::fs::create_dir_all(directory)?;
                watcher.watch(directory, notify::RecursiveMode::NonRecursive)?;
                let mut themes_watched = false;
                let mut work = Work::Reload; // Close the startup read/watch gap.
                loop {
                    match work {
                        Work::Reload => {
                            if themes_watched {
                                let _unwatched = watcher.unwatch(&theme_directory);
                                themes_watched = false;
                            }
                            if theme_directory.is_dir() {
                                watcher
                                    .watch(&theme_directory, notify::RecursiveMode::Recursive)?;
                                themes_watched = true;
                            }
                            match selection.reload() {
                                Ok(Some(next)) => {
                                    proxy.send_event(ClientEvent::ThemeChanged(next))?;
                                }
                                Ok(None) => {}
                                Err(error) => eprintln!(
                                    "THEME_REJECTED active_revision={}: {error:#}",
                                    selection.active.revision
                                ),
                            }
                        }
                        Work::Request(request) => {
                            let response = serde_json::json!({
                                "type": "theme-response", "request": request.request,
                                "operation": request.operation, "success": true,
                            });
                            let mut response = response
                                .as_object()
                                .context("theme response object")?
                                .clone();
                            match selection.apply(&request) {
                                Ok((next, details)) => {
                                    if let Some(next) = next {
                                        proxy.send_event(ClientEvent::ThemeChanged(next))?;
                                    }
                                    response.extend(
                                        details
                                            .as_object()
                                            .context("theme response details")?
                                            .clone(),
                                    );
                                }
                                Err(error) => {
                                    eprintln!("THEME_REJECTED {}: {error:#}", request.operation);
                                    response.insert("success".into(), false.into());
                                    response.insert("message".into(), format!("{error:#}").into());
                                    // Failed saves must not leave an uncommitted preview active.
                                    if request.operation == "commit" {
                                        selection.preview = None;
                                        if let Some(next) =
                                            selection.activate(selection.committed.clone())?
                                        {
                                            proxy.send_event(ClientEvent::ThemeChanged(next))?;
                                        }
                                    }
                                }
                            }
                            proxy.send_event(ClientEvent::ThemeReply(serde_json::to_vec(
                                &response,
                            )?))?;
                        }
                    }
                    let Ok(next) = receiver.recv() else { break };
                    work = next;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("theme loader stopped: {error:#}");
            }
        })?;
    Ok((initial, Controller(sender)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(operation: &str, name: Option<&str>) -> ThemeCommand {
        ThemeCommand {
            request: 1,
            operation: operation.into(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn previews_are_memory_only_and_cancel_uses_latest_saved_theme() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("brood.toml");
        let original = "# Keep my settings\ntheme = 'ashen'\n[init]\nclient = 'custom.wasm'\n";
        std::fs::write(&config, original)?;
        let mut selection = Selection::new(config.clone())?;
        let first = selection
            .apply(&request("preview", Some("github_light")))?
            .0
            .unwrap();
        assert_eq!(std::fs::read_to_string(&config)?, original);
        assert!(selection.reload()?.is_none());
        save(&config, "gruvbox")?;
        assert!(selection.reload()?.is_none());
        let restored = selection.apply(&request("cancel", None))?.0.unwrap();
        assert_eq!(restored.name, "gruvbox");
        assert!(restored.revision > first.revision);
        selection.apply(&request("preview", Some("github_light")))?;
        selection.apply(&request("preview", None))?;
        assert_eq!(selection.active.name, "gruvbox");
        Ok(())
    }

    #[test]
    fn commit_preserves_config_and_survives_restart_invalid_input_is_transactional() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("brood.toml");
        std::fs::write(
            &config,
            "# Keep this comment\n[init]\nclient = 'custom.wasm'\n",
        )?;
        let mut selection = Selection::new(config.clone())?;
        selection.apply(&request("preview", Some("github_light")))?;
        selection.apply(&request("commit", Some("github_light")))?;
        assert!(selection.reload()?.is_none());
        let saved = std::fs::read_to_string(&config)?;
        assert!(saved.contains("# Keep this comment") && saved.contains("client = 'custom.wasm'"));
        assert_eq!(Selection::new(config.clone())?.active.name, "github_light");
        for operation in ["preview", "commit"] {
            assert!(
                selection
                    .apply(&request(operation, Some("missing")))
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&config)?, saved);
            assert_eq!(selection.active.name, "github_light");
        }
        // A malformed concurrent edit fails the save, preserving the file.
        std::fs::write(&config, "[broken")?;
        assert!(selection.apply(&request("commit", Some("ashen"))).is_err());
        assert_eq!(std::fs::read_to_string(&config)?, "[broken");
        Ok(())
    }

    #[test]
    fn catalogue_includes_bundled_and_user_themes_with_user_precedence() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("brood.toml");
        std::fs::create_dir(dir.path().join("themes"))?;
        std::fs::write(
            dir.path().join("themes/custom.toml"),
            "inherits = 'github_light'\n",
        )?;
        std::fs::write(
            dir.path().join("themes/ashen.toml"),
            "inherits = 'github_light'\n",
        )?;
        let mut selection = Selection::new(config)?;
        let (_, catalogue) = selection.apply(&request("list", None))?;
        let names = catalogue["names"].as_array().unwrap();
        assert_eq!(names.len(), brood_theme::BUNDLED.len() + 1);
        assert!(names.contains(&serde_json::json!("custom")));
        assert_eq!(
            selection.active.roles,
            selection.loader.load("github_light", 1)?.roles
        );
        Ok(())
    }
}
