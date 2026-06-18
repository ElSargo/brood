mod browser;
mod compositor;
mod control;
mod input;
mod presentation;
mod state;
mod wasm_key;

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

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
    /// Run a nested Wayland compositor and attach a presentation window.
    Run(RunArgs),
    /// Send a command to a running manifoldd control socket.
    Ctl(CtlArgs),
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Command to launch as a hosted Wayland app inside the nested compositor.
    #[arg(short = 'c', long = "command")]
    command: Option<OsString>,
    /// Path for the session control socket.
    #[arg(long, default_value = "/tmp/manifoldd.sock")]
    control_socket: PathBuf,
    /// Start without an attached presentation window.
    #[arg(long)]
    headless: bool,
    /// Launch an isolated Chromium browser pane at startup.
    #[arg(long = "browser", value_name = "URL", num_args = 0..=1, default_missing_value = "about:blank")]
    browser: Option<String>,
    /// Load a core-WASM normal-mode key handler.
    #[arg(long = "wasm-key-handler", value_name = "PATH")]
    wasm_key_handler: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct CtlArgs {
    /// Path for the session control socket.
    #[arg(long, default_value = "/tmp/manifoldd.sock")]
    control_socket: PathBuf,
    /// Send COMMAND as a raw JSON argv array instead of parsing command-line words.
    #[arg(long)]
    json: bool,
    /// Command to send: attach, detach, close-frontend, spawn <cmd>, browser, inject, status, quit.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

fn main() -> Result<()> {
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let args = Args::parse();
    info!(?args, "starting manifoldd");

    match args.command.unwrap_or(Command::Check) {
        Command::Check => {
            println!("manifoldd: ok");
            Ok(())
        }
        Command::Run(args) => compositor::run(args),
        Command::Ctl(args) => control::send(args).context("send control command"),
    }
}
