use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

pub const MANIFOLD_SESSION_ENV: &str = "manifold_session";
pub const DEFAULT_MANIFOLD_DBUS_NAME: &str = "org.manifold.Session";
pub const MANIFOLD_HOST_DBUS_ADDRESS_ENV: &str = "MANIFOLD_HOST_DBUS_ADDRESS";
pub const MANIFOLDD_DBUS_ADDRESS_ENV: &str = "MANIFOLDD_DBUS_ADDRESS";
pub const MANIFOLDD_DBUS_DESTINATION_ENV: &str = "MANIFOLDD_DBUS_DESTINATION";
pub const MANIFOLD_PANE_ID_ENV: &str = "MANIFOLD_PANE_ID";

#[derive(Debug, Parser)]
#[command(about = "Manifold command line", version)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
    /// Run a Manifold session server directly.
    Run(RunArgs),
    /// Hot-load a WASM module into a running Manifold session.
    Plug(PlugArgs),
    /// Select a live session with skim and present it.
    Switch(SwitchArgs),
    /// Send a command to a Manifold `DBus` control endpoint.
    Ctl(CtlArgs),
}

#[derive(Debug, Clone, Parser, Default)]
pub struct RunArgs {
    #[command(flatten)]
    pub session: SessionArgs,
    /// Bypass the configured session runner for this launch.
    #[arg(long = "no-session-runner")]
    pub no_session_runner: bool,
    /// Override the configured session runner for this launch.
    #[arg(long = "session-runner", value_name = "KIND")]
    pub session_runner: Option<String>,
}

#[derive(Debug, Clone, Parser, serde::Deserialize, serde::Serialize)]
pub struct SessionArgs {
    /// Command to launch as a hosted Wayland app inside the nested compositor.
    #[arg(short = 'c', long = "command")]
    pub command: Option<OsString>,
    /// `DBus` destination name for this session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name", default_value = DEFAULT_MANIFOLD_DBUS_NAME)]
    pub dbus_name: String,
    /// `DBus` bus address for this session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Start the service/runtime process without tying it to a presentation session.
    #[arg(long)]
    pub headless: bool,
    /// Start with an attached offscreen presentation target. Used by nested tests.
    #[arg(long = "offscreen-presentation", hide = true)]
    pub offscreen_presentation: bool,
    /// Render test fiducials in each hosted surface's local coordinate space.
    #[arg(long = "test-surface-markers", hide = true)]
    pub test_surface_markers: bool,
    /// Launch an isolated Chromium browser pane at startup.
    #[arg(long = "browser", value_name = "URL", num_args = 0..=1, default_missing_value = "about:blank")]
    pub browser: Option<String>,
    /// Load a self-contained target-shape WASM plugin module.
    #[arg(long = "wasm-plugin", value_name = "PATH")]
    pub wasm_plugin: Option<PathBuf>,
    /// Load a target-shape WASM key policy provider as NAME from PATH.
    #[arg(long = "wasm-key-policy", value_names = ["NAME", "PATH"], num_args = 2)]
    pub wasm_key_policy: Option<Vec<String>>,
    /// Load an async core-WASM test harness.
    #[arg(long = "test-harness", value_name = "PATH")]
    pub test_harness: Option<PathBuf>,
    /// Inner `DBus` control destination used by nested test supervisors.
    #[arg(
        long = "nested-inner-dbus-destination",
        alias = "nested-inner-dbus-name",
        value_name = "NAME",
        hide = true
    )]
    pub nested_inner_dbus_name: Option<String>,
    /// Inner `DBus` bus address used by nested test supervisors.
    #[arg(
        long = "nested-inner-dbus-address",
        value_name = "ADDRESS",
        hide = true
    )]
    pub nested_inner_dbus_address: Option<String>,
    /// Config file to load at startup.
    #[arg(long = "config", value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Start without loading modules from the config file.
    #[arg(long = "no-default-modules")]
    pub no_default_modules: bool,
    /// Start without creating the default surface session. Used by auto-started control servers.
    #[arg(long = "no-initial-surface-session", hide = true)]
    pub no_initial_surface_session: bool,
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
            wasm_plugin: None,
            wasm_key_policy: None,
            test_harness: None,
            nested_inner_dbus_name: None,
            nested_inner_dbus_address: None,
            config: None,
            no_default_modules: false,
            no_initial_surface_session: false,
        }
    }
}

#[derive(Debug, Parser)]
pub struct EnterArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Force creation of a new session even when already entered.
    #[arg(long = "new")]
    pub new: bool,
    /// Make the selected or newly created session the default for this directory.
    #[arg(long = "set-default")]
    pub set_default: bool,
    /// Keep GUI apps launched from the entered environment on the inherited display.
    #[arg(long = "no-capture")]
    pub no_capture: bool,
    /// Command to run inside the entered environment. Defaults to $SHELL.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, Parser)]
pub struct ShowArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Show the most recently focused live session instead of this directory's default session.
    #[arg(long = "last-focused", conflicts_with_all = ["id", "set_default"])]
    pub last_focused: bool,
    /// Show this session id or session name directly.
    #[arg(long = "id")]
    pub id: Option<String>,
    /// Make the shown session the default for this directory.
    #[arg(long = "set-default")]
    pub set_default: bool,
    /// Show this directory's session even when already inside another Manifold session.
    #[arg(value_enum, value_name = "TARGET", conflicts_with_all = ["id", "last_focused"])]
    pub target: Option<ShowTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShowTarget {
    Here,
}

#[derive(Debug, Parser)]
pub struct HideArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Hide this session id or session name directly.
    #[arg(long = "id")]
    pub id: Option<String>,
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// List all active sessions instead of only sessions associated with this directory.
    #[arg(long = "global")]
    pub global: bool,
}

#[derive(Debug, Parser)]
pub struct PlugArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// WASM lane to load. Auto detects a plugin artifact; key-policy selects a policy provider.
    #[arg(long = "kind", value_name = "KIND", default_value = "auto")]
    pub kind: String,
    /// Stable actor instance id. Defaults to the module file stem.
    #[arg(long = "id", value_name = "INSTANCE_ID")]
    pub id: Option<String>,
    /// WASM module path to load into the running session.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

#[derive(Debug, Parser)]
pub struct SwitchArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Skim-compatible selector command.
    #[arg(long = "skim", default_value = "sk")]
    pub skim: OsString,
    /// Select this session directly instead of opening skim.
    #[arg(long = "session-id")]
    pub session_id: Option<u64>,
    /// Initial session name/query. If it matches nothing, a new session is created with this name.
    #[arg(value_name = "NAME")]
    pub query: Vec<String>,
}

/// The process-level result of executing one parsed CLI command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    Completed,
    Exit(i32),
}

#[derive(Debug, Parser)]
#[command(about = "Manifold presentation client", version)]
pub struct ClientArgs {
    /// Start the presentation client without connecting to or starting a session server.
    #[arg(
        long = "no-server",
        conflicts_with_all = ["dbus_name", "dbus_address", "server"]
    )]
    pub no_server: bool,
    /// `DBus` destination name for the session server.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session server.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Override the session server executable used for auto-start.
    #[arg(long = "server", value_name = "PATH")]
    pub server: Option<PathBuf>,
    /// Start a presentation-scoped WASM actor as ID from PATH.
    #[arg(long = "wasm-actor", value_name = "ID=PATH")]
    pub wasm_actors: Vec<String>,
    /// Load a client-local WASM layout policy provider as NAME from PATH.
    #[arg(long = "wasm-layout-policy", value_names = ["NAME", "PATH"], num_args = 2)]
    pub wasm_layout_policy: Option<Vec<String>>,
    /// Validate and invoke the configured layout policy without opening a presentation.
    #[arg(
        long = "check-wasm-layout-policy",
        hide = true,
        requires = "wasm_layout_policy"
    )]
    pub check_wasm_layout_policy: bool,
    /// Validate configured presentation actors without opening a presentation.
    #[arg(long = "check-wasm-actors", hide = true, requires = "wasm_actors")]
    pub check_wasm_actors: bool,
}

#[cfg(test)]
mod client_args_tests {
    use super::*;

    #[test]
    fn no_server_selects_standalone_client_startup() {
        let parsed = ClientArgs::try_parse_from(["manifold-client", "--no-server"]);
        assert!(parsed.is_ok());
        if let Ok(args) = parsed {
            assert!(args.no_server);
            assert!(args.dbus_name.is_none());
            assert!(args.dbus_address.is_none());
            assert!(args.server.is_none());
            assert!(args.wasm_layout_policy.is_none());
            assert!(!args.check_wasm_layout_policy);
        }
    }

    #[test]
    fn layout_policy_keeps_provider_and_path_together() {
        let parsed = ClientArgs::try_parse_from([
            "manifold-client",
            "--no-server",
            "--wasm-layout-policy",
            "tile",
            "/tmp/layout.wasm",
        ]);
        assert!(parsed.is_ok());
        if let Ok(args) = parsed {
            assert_eq!(
                args.wasm_layout_policy.as_deref(),
                Some(&["tile".to_string(), "/tmp/layout.wasm".to_string()][..])
            );
        }
    }

    #[test]
    fn no_server_rejects_connection_options() {
        assert!(
            ClientArgs::try_parse_from([
                "manifold-client",
                "--no-server",
                "--dbus-destination",
                "org.manifold.Test",
            ])
            .is_err()
        );
        assert!(
            ClientArgs::try_parse_from([
                "manifold-client",
                "--no-server",
                "--server",
                "/tmp/manifoldd",
            ])
            .is_err()
        );
    }
}

#[derive(Debug, Parser)]
pub struct CtlArgs {
    /// `DBus` destination name for the session control endpoint.
    #[arg(long = "dbus-destination", alias = "dbus-name")]
    pub dbus_name: Option<String>,
    /// `DBus` bus address for the session control endpoint.
    #[arg(long = "dbus-address")]
    pub dbus_address: Option<String>,
    /// Send COMMAND as a raw JSON argv array instead of parsing command-line words.
    #[arg(long)]
    pub json: bool,
    /// Command to send: attach, detach, close-frontend, close-session, client close, spawn <cmd>, actors, browser, inject, status, quit.
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}
