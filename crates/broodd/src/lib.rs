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
mod frame_export;
mod geometry;
mod input;
pub mod launch_context;
mod nested_test;
mod plugin_runtime;
mod runtime;
mod session_bus;
mod session_bus_state;
mod session_runner;
pub(crate) mod state;
pub(crate) mod state_services;
mod wasm_test;

use anyhow::{Context, Result};
use brood_cli::{Args, Command};
pub use brood_cli::{
    BROOD_HOST_DBUS_ADDRESS_ENV, BROOD_PANE_ID_ENV, BROOD_SESSION_ENV, BROODD_DBUS_ADDRESS_ENV,
    BROODD_DBUS_DESTINATION_ENV, CommandOutcome, CtlArgs, DEFAULT_BROOD_DBUS_NAME, EnterArgs,
    HideArgs, ListArgs, PlugArgs, RunArgs, SessionArgs, ShowArgs, ShowTarget, SwitchArgs, TestArgs,
};
use clap::Parser;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

pub fn run_cli() -> Result<CommandOutcome> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let args = Args::parse();
    info!(?args, "starting brood");

    let Some(command) = args.command else {
        let status = brood_cli::control::launch_client().context("launch presentation client")?;
        return status.code().map_or_else(
            || Err(anyhow::anyhow!("presentation client terminated by signal")),
            |code| Ok(CommandOutcome::Exit(code)),
        );
    };

    match command {
        Command::Check => {
            println!("brood: ok");
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
        Command::Test(args) => nested_test::run(args)
            .context("run checked-in behavior proof")
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
    info!(?args, "starting brood session server");
    runtime::serve_session(args)
}
