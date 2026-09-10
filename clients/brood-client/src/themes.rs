//! Config-driven, transactional theme selection and filesystem reloads.
use crate::ClientEvent;
use anyhow::{Context, Result};
use brood_graphics::theme::ThemeSnapshot;
use brood_theme::ThemeLoader;
use notify::Watcher;
use std::{path::PathBuf, sync::mpsc, time::Duration};
use winit::event_loop::EventLoopProxy;

fn load(config: &std::path::Path, revision: u64) -> Result<ThemeSnapshot> {
    let mut name = "ashen".to_string();
    if config.exists() {
        let contents = std::fs::read_to_string(config).context("read theme selection config")?;
        let table: toml::Table =
            toml::from_str(&contents).context("parse theme selection config")?;
        if let Some(value) = table.get("theme") {
            name = value
                .as_str()
                .context("theme must be a Helix theme name")?
                .to_string();
        }
    }
    ThemeLoader {
        directories: vec![config.with_file_name("themes")],
    }
    .load(&name, revision)
}

/// The worker owns the watcher and last valid revision. Parse failures never
/// enter presentation state. Watching the directory also covers atomic saves.
pub fn watch(config: PathBuf, proxy: EventLoopProxy<ClientEvent>) -> Result<ThemeSnapshot> {
    let config = std::path::absolute(config)?;
    let fallback = ThemeLoader::default().load("ashen", 1)?;
    let initial = load(&config, 1).unwrap_or_else(|error| {
        eprintln!("THEME_REJECTED startup: {error:#}");
        fallback
    });
    let mut active = initial.clone();
    std::thread::Builder::new()
        .name("brood-theme-loader".into())
        .spawn(move || {
            let (sender, receiver) = mpsc::sync_channel(1);
            let watched_config = config.clone();
            let watched_themes = config.with_file_name("themes");
            let watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if event.as_ref().is_ok_and(|event| {
                        !matches!(event.kind, notify::EventKind::Access(_))
                            && event.paths.iter().any(|path| {
                                path == &watched_config || path.starts_with(&watched_themes)
                            })
                    }) {
                        let _queued = sender.try_send(());
                    }
                });
            let result = (|| -> Result<()> {
                let mut watcher = watcher?;
                let directory = config
                    .parent()
                    .context("theme config needs a parent directory")?;
                std::fs::create_dir_all(directory)
                    .context("create theme configuration directory")?;
                watcher.watch(directory, notify::RecursiveMode::NonRecursive)?;
                let theme_directory = config.with_file_name("themes");
                let mut themes_watched = false;
                // Close the startup read/watch gap.
                loop {
                    // A theme directory may itself be replaced atomically.
                    if themes_watched {
                        let _unwatched = watcher.unwatch(&theme_directory);
                        themes_watched = false;
                    }
                    if theme_directory.is_dir() {
                        watcher.watch(&theme_directory, notify::RecursiveMode::Recursive)?;
                        themes_watched = true;
                    }
                    match load(&config, active.revision.saturating_add(1)) {
                        Ok(next) if next.name != active.name || next.roles != active.roles => {
                            if proxy
                                .send_event(ClientEvent::ThemeChanged(next.clone()))
                                .is_err()
                            {
                                break;
                            }
                            active = next;
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!(
                            "THEME_REJECTED active_revision={}: {error:#}",
                            active.revision
                        ),
                    }
                    if receiver.recv().is_err() {
                        break;
                    }
                    // Coalesce editor saves; no actor or event-loop thread blocks.
                    while receiver.recv_timeout(Duration::from_millis(30)).is_ok() {}
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("theme watcher stopped: {error:#}");
            }
        })?;
    Ok(initial)
}
