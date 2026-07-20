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
mod geometry;
mod input;
pub mod launch_context;
mod layout;
mod plugin_runtime;
mod presentation;
mod runtime;
mod session_bus;
mod session_bus_state;
mod session_runner;
pub(crate) mod state;
pub(crate) mod state_services;
mod wasm_test;
mod winit_presenter;

use anyhow::{Context, Result};
use clap::Parser;
use manifold_cli::{Args, Command};
pub use manifold_cli::{
    CommandOutcome, CtlArgs, DEFAULT_MANIFOLD_DBUS_NAME, EnterArgs, HideArgs, ListArgs,
    MANIFOLD_HOST_DBUS_ADDRESS_ENV, MANIFOLD_PANE_ID_ENV, MANIFOLD_SESSION_ENV,
    MANIFOLDD_DBUS_ADDRESS_ENV, MANIFOLDD_DBUS_DESTINATION_ENV, PlugArgs, RunArgs, SessionArgs,
    ShowArgs, ShowTarget, SwitchArgs,
};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

pub fn run_cli() -> Result<CommandOutcome> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let args = Args::parse();
    info!(?args, "starting manifold");

    let Some(command) = args.command else {
        let status =
            manifold_cli::control::launch_client().context("launch presentation client")?;
        return status.code().map_or_else(
            || Err(anyhow::anyhow!("presentation client terminated by signal")),
            |code| Ok(CommandOutcome::Exit(code)),
        );
    };

    match command {
        Command::Check => {
            println!("manifold: ok");
            Ok(CommandOutcome::Completed)
        }
        Command::Enter(args) => control::enter(args).context("enter session environment"),
        Command::Show(args) => control::show(args)
            .context("show session environment")
            .map(|()| CommandOutcome::Completed),
        Command::Hide(args) => control::hide(args)
            .context("hide session environment")
            .map(|()| CommandOutcome::Completed),
        Command::List(args) => control::list(args)
            .context("list session environments")
            .map(|()| CommandOutcome::Completed),
        Command::Run(args) => runtime::run(args).map(|()| CommandOutcome::Completed),
        Command::Plug(args) => control::plug(args)
            .context("plug WASM module")
            .map(|()| CommandOutcome::Completed),
        Command::Switch(args) => control::switch(args)
            .context("switch session")
            .map(|()| CommandOutcome::Completed),
        Command::Ctl(args) => control::send(args)
            .context("send control command")
            .map(|()| CommandOutcome::Completed),
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
    runtime::serve_session(args)
}
