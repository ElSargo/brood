use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "brood capabilities",
    about = "Report compiled capabilities as JSON"
)]
struct CapabilitiesArgs {}

fn main() -> anyhow::Result<ExitCode> {
    let result = run().and_then(|outcome| match outcome {
        broodd::CommandOutcome::Completed => Ok(ExitCode::SUCCESS),
        broodd::CommandOutcome::Exit(code) => u8::try_from(code)
            .with_context(|| format!("command returned unsupported exit code {code}"))
            .map(ExitCode::from),
    });
    brood_profiler::flush();
    result
}

fn run() -> anyhow::Result<broodd::CommandOutcome> {
    use broodd::CommandOutcome;
    match brood_launch::role() {
        Some("session") => broodd::run_session_cli()?,
        Some("remote") => brood_remote::run_cli()?,
        Some("capsule") => brood_capsuled::cli::run_cli()?,
        Some("computer-use-mcp") => broodd::computer_use_mcp::run_cli()?,
        Some("runner-exec") => broodd::runner_exec::run_cli()?,
        Some("runner-nix-develop") => {
            return Ok(CommandOutcome::Exit(i32::from(
                broodd::runner_nix_develop::run_cli()?.shell_code(),
            )));
        }
        Some("bmux") => {
            return Ok(CommandOutcome::Exit(i32::from(
                bmux::run_cli() != ExitCode::SUCCESS,
            )));
        }
        Some("profile") => brood_profiler::cli::run_cli()?,
        Some("client") => {
            #[cfg(feature = "presentation")]
            brood_client::run_cli()?;
            #[cfg(not(feature = "presentation"))]
            anyhow::bail!(
                "this Brood build does not include presentation support; use the full build"
            );
        }
        _ => {
            if std::env::args_os()
                .nth(1)
                .is_some_and(|arg| arg == "capabilities")
            {
                CapabilitiesArgs::parse_from(std::env::args_os().skip(1));
                println!(
                    "{}",
                    serde_json::json!({
                        "server": true, "remote": true, "capsule": true,
                        "computer-use-mcp": true, "presentation": cfg!(feature = "presentation")
                    })
                );
            } else {
                return broodd::run_cli();
            }
        }
    }
    Ok(CommandOutcome::Completed)
}
