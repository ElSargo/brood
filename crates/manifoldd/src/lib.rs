#![allow(
    clippy::option_if_let_else,
    reason = "tracked in issues.md: CLI command normalization still uses local option branching"
)]
// TODO(lint-structure): move CLI/default resolution into typed helpers instead of ad hoc options.

mod browser;
mod clipboard;
mod command;
mod compositor;
mod config;
mod control;
mod debug_dump;
mod input;
pub mod launch_context;
mod layout;
mod presentation;
mod session_bus;
mod session_runner;
mod state;
mod wasm_key;
mod wasm_map;
mod wasm_test;
mod winit_presenter;

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

pub(crate) const MANIFOLD_SESSION_ENV: &str = "manifold_session";
pub(crate) const DEFAULT_MANIFOLD_DBUS_NAME: &str = "org.manifold.Session";
pub(crate) const MANIFOLD_HOST_DBUS_ADDRESS_ENV: &str = "MANIFOLD_HOST_DBUS_ADDRESS";
pub(crate) const MANIFOLDD_DBUS_ADDRESS_ENV: &str = "MANIFOLDD_DBUS_ADDRESS";
pub(crate) const MANIFOLDD_DBUS_DESTINATION_ENV: &str = "MANIFOLDD_DBUS_DESTINATION";
pub(crate) const MANIFOLD_PANE_ID_ENV: &str = "MANIFOLD_PANE_ID";

#[derive(Debug, Parser)]
#[command(about = "Manifold session process", version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print startup information and exit.
    Check,
    /// Enter a new Manifold session environment for a shell or command.
    Enter(EnterArgs),
    /// Show the current Manifold session in the active presentation surface.
    Show(ShowArgs),
    /// Hide the current Manifold session presentation surface if it exists.
    Hide(HideArgs),
    /// List Manifold sessions for this directory or all sessions.
    List(ListArgs),
    /// Run a Manifold session process with an attached presentation by default.
    Run(RunArgs),
    /// Hot-load a WASM module into a running Manifold session.
    Plug(PlugArgs),
    /// Select a live session with skim and present it.
    Switch(SwitchArgs),
    /// Run built-in test workflows. Defaults to the nested topology.
    Test(TestArgs),
    /// Send a command to a Manifold `DBus` control endpoint.
    Ctl(CtlArgs),
}

#[derive(Debug, Clone, Parser, Default)]
struct RunArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// Bypass the configured session runner for this launch.
    #[arg(long = "no-session-runner")]
    no_session_runner: bool,
    /// Override the configured session runner for this launch.
    #[arg(long = "session-runner", value_name = "KIND")]
    session_runner: Option<String>,
}

#[derive(Debug, Clone, Parser, serde::Deserialize, serde::Serialize)]
pub struct SessionArgs {
    /// Command to launch as a hosted Wayland app inside the nested compositor.
    #[arg(short = 'c', long = "command")]
    pub(crate) command: Option<OsString>,
    /// `DBus` destination name for this session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name", default_value = DEFAULT_MANIFOLD_DBUS_NAME)]
    pub(crate) dbus_name: String,
    /// `DBus` bus address for this session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// Start the service/runtime process without tying it to a presentation session.
    #[arg(long)]
    pub(crate) headless: bool,
    /// Start with an attached offscreen presentation target. Used by nested tests.
    #[arg(long = "offscreen-presentation", hide = true)]
    pub(crate) offscreen_presentation: bool,
    /// Render test fiducials in each hosted surface's local coordinate space.
    #[arg(long = "test-surface-markers", hide = true)]
    pub(crate) test_surface_markers: bool,
    /// Launch an isolated Chromium browser pane at startup.
    #[arg(long = "browser", value_name = "URL", num_args = 0..=1, default_missing_value = "about:blank")]
    pub(crate) browser: Option<String>,
    /// Load a core-WASM normal-mode key handler.
    #[arg(long = "wasm-key-handler", value_name = "PATH")]
    pub(crate) wasm_key_handler: Option<PathBuf>,
    /// Load a core-WASM fast map hook.
    #[arg(long = "wasm-map-hook", value_name = "PATH")]
    pub(crate) wasm_map_hook: Option<PathBuf>,
    /// Load an async core-WASM test harness.
    #[arg(long = "test-harness", value_name = "PATH")]
    pub(crate) test_harness: Option<PathBuf>,
    /// Inner `DBus` control destination used by nested test supervisors.
    #[arg(
        long = "nested-inner-dbus-destination",
        alias = "nested-inner-dbus-name",
        value_name = "NAME",
        hide = true
    )]
    pub(crate) nested_inner_dbus_name: Option<String>,
    /// Inner `DBus` bus address used by nested test supervisors.
    #[arg(
        long = "nested-inner-dbus-address",
        value_name = "ADDRESS",
        hide = true
    )]
    pub(crate) nested_inner_dbus_address: Option<String>,
    /// Config file to load at startup.
    #[arg(long = "config", value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,
    /// Start without loading modules from the config file.
    #[arg(long = "no-default-modules")]
    pub(crate) no_default_modules: bool,
    /// Start without creating the default surface session. Used by auto-started control servers.
    #[arg(long = "no-initial-surface-session", hide = true)]
    pub(crate) no_initial_surface_session: bool,
}

impl Default for SessionArgs {
    fn default() -> Self {
        Self {
            command: None,
            dbus_name: DEFAULT_MANIFOLD_DBUS_NAME.to_string(),
            dbus_address: None,
            headless: false,
            offscreen_presentation: false,
            test_surface_markers: false,
            browser: None,
            wasm_key_handler: None,
            wasm_map_hook: None,
            test_harness: None,
            nested_inner_dbus_name: None,
            nested_inner_dbus_address: None,
            config: None,
            no_default_modules: false,
            no_initial_surface_session: false,
        }
    }
}

impl SessionArgs {
    pub(crate) fn resolved_config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(config::global_config_path)
    }
}

#[derive(Debug, Parser)]
pub(crate) struct EnterArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// Force creation of a new session even when already entered.
    #[arg(long = "new")]
    pub(crate) new: bool,
    /// Make the selected or newly created session the default for this directory.
    #[arg(long = "set-default")]
    pub(crate) set_default: bool,
    /// Keep GUI apps launched from the entered environment on the inherited display.
    #[arg(long = "no-capture")]
    pub(crate) no_capture: bool,
    /// Command to run inside the entered environment. Defaults to $SHELL.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) command: Vec<OsString>,
}

#[derive(Debug, Parser)]
pub(crate) struct ShowArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// Show the most recently focused live session instead of this directory's default session.
    #[arg(long = "last-focused", conflicts_with_all = ["id", "set_default"])]
    pub(crate) last_focused: bool,
    /// Show this session id or session name directly.
    #[arg(long = "id")]
    pub(crate) id: Option<String>,
    /// Make the shown session the default for this directory.
    #[arg(long = "set-default")]
    pub(crate) set_default: bool,
    /// Show this directory's session even when already inside another Manifold session.
    #[arg(value_enum, value_name = "TARGET", conflicts_with_all = ["id", "last_focused"])]
    pub(crate) target: Option<ShowTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ShowTarget {
    Here,
}

#[derive(Debug, Parser)]
pub(crate) struct HideArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// Hide this session id or session name directly.
    #[arg(long = "id")]
    pub(crate) id: Option<String>,
}

#[derive(Debug, Parser)]
pub(crate) struct ListArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// List all active sessions instead of only sessions associated with this directory.
    #[arg(long = "global")]
    pub(crate) global: bool,
}

#[derive(Debug, Parser)]
pub(crate) struct PlugArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// WASM lane to replace. Auto tries the key handler ABI, then the map hook ABI.
    #[arg(long = "kind", value_name = "KIND", default_value = "auto")]
    pub(crate) kind: String,
    /// WASM module path to load into the running session.
    #[arg(value_name = "PATH")]
    pub(crate) path: PathBuf,
}

#[derive(Debug, Parser)]
pub(crate) struct SwitchArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub(crate) dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub(crate) dbus_address: Option<String>,
    /// Skim-compatible selector command.
    #[arg(long = "skim", default_value = "sk")]
    pub(crate) skim: OsString,
    /// Select this session directly instead of opening skim.
    #[arg(long = "session-id")]
    pub(crate) session_id: Option<u64>,
    /// Initial session name/query. If it matches nothing, a new session is created with this name.
    #[arg(value_name = "NAME")]
    pub(crate) query: Vec<String>,
}

#[derive(Debug, Parser)]
struct TestArgs {
    #[command(subcommand)]
    command: Option<TestCommand>,
}

#[derive(Debug, Subcommand)]
enum TestCommand {
    /// Launch a nested Manifold-in-Manifold test topology.
    Nested(NestedTestArgs),
}

#[derive(Debug, Parser, Default)]
struct NestedTestArgs {
    /// Directory for sockets, logs, and artifacts.
    #[arg(long = "out-dir", value_name = "DIR")]
    out_dir: Option<PathBuf>,
    /// Keep the nested topology running after startup.
    #[arg(long = "keep-running")]
    keep_running: bool,
    /// Load the async test harness into the test supervisor or subject instance.
    #[arg(long = "harness", value_name = "PATH")]
    harness: Option<PathBuf>,
    /// Start the outer supervisor with a visible presentation window.
    #[arg(long = "outer-headful")]
    outer_headful: bool,
    /// Start an outer supervisor even when `manifold_session` says this is already nested.
    #[arg(long = "force-outer")]
    force_outer: bool,
    /// Start the inner system under test without an attached presentation window.
    #[arg(long = "inner-headless")]
    inner_headless: bool,
    /// Load a core-WASM map hook into the inner system under test.
    #[arg(long = "inner-wasm-map-hook", value_name = "PATH")]
    inner_wasm_map_hook: Option<PathBuf>,
    /// Load a core-WASM key handler into the inner system under test.
    #[arg(long = "inner-wasm-key-handler", value_name = "PATH")]
    inner_wasm_key_handler: Option<PathBuf>,
    /// Spawn a hosted Wayland app inside the inner instance after startup.
    #[arg(long = "inner-command", value_name = "CMD")]
    inner_command: Option<String>,
}

#[derive(Debug, Parser)]
struct CtlArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    dbus_address: Option<String>,
    /// Send COMMAND as a raw JSON argv array instead of parsing command-line words.
    #[arg(long)]
    json: bool,
    /// Command to send: attach, detach, close-frontend, close-session, client close, spawn <cmd>, browser, inject, status, quit.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

pub fn run_cli() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let args = Args::parse();
    info!(?args, "starting manifold");

    let command = match args.command {
        Some(command) => command,
        None => Command::Run(RunArgs::default()),
    };

    match command {
        Command::Check => {
            println!("manifold: ok");
            Ok(())
        }
        Command::Enter(args) => control::enter(args).context("enter session environment"),
        Command::Show(args) => control::show(args).context("show session environment"),
        Command::Hide(args) => control::hide(args).context("hide session environment"),
        Command::List(args) => control::list(args).context("list session environments"),
        Command::Run(args) => compositor::run(args),
        Command::Plug(args) => control::plug(args).context("plug WASM module"),
        Command::Switch(args) => control::switch(args).context("switch session"),
        Command::Test(args) => compositor::run_test(args),
        Command::Ctl(args) => control::send(args).context("send control command"),
    }
}

pub fn run_session_cli() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let args = match launch_context::current_launch_record() {
        Ok(record) => record.session_args.unwrap_or_default(),
        Err(_) => SessionArgs::parse(),
    };
    info!(?args, "starting manifold session server");
    compositor::serve_session(args)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "CLI parser tests intentionally panic on impossible command variants"
)]
mod tests {
    use super::{Args, Command, ShowTarget};
    use clap::Parser;

    #[test]
    fn show_accepts_last_focused_mode() {
        let args = Args::parse_from(["manifold", "show", "--last-focused"]);

        match args.command {
            Some(Command::Show(show)) => assert!(show.last_focused),
            other => panic!("expected show command, got {other:?}"),
        }
    }

    #[test]
    fn show_last_focused_conflicts_with_explicit_session() {
        let err = Args::try_parse_from(["manifold", "show", "--last-focused", "--id", "2"])
            .expect_err("last-focused and id should conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn show_last_focused_conflicts_with_set_default() {
        let err = Args::try_parse_from(["manifold", "show", "--last-focused", "--set-default"])
            .expect_err("last-focused and set-default should conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn show_accepts_here_target() {
        let args = Args::parse_from(["manifold", "show", "here"]);

        match args.command {
            Some(Command::Show(show)) => assert_eq!(show.target, Some(ShowTarget::Here)),
            other => panic!("expected show command, got {other:?}"),
        }
    }

    #[test]
    fn show_here_conflicts_with_explicit_session() {
        let err = Args::try_parse_from(["manifold", "show", "here", "--id", "2"])
            .expect_err("here and id should conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn show_here_conflicts_with_last_focused() {
        let err = Args::try_parse_from(["manifold", "show", "here", "--last-focused"])
            .expect_err("here and last-focused should conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
