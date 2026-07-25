use std::process::ExitCode;

use anyhow::Context;

fn main() -> anyhow::Result<ExitCode> {
    match broodd::run_cli()? {
        broodd::CommandOutcome::Completed => Ok(ExitCode::SUCCESS),
        broodd::CommandOutcome::Exit(code) => {
            let code = u8::try_from(code)
                .with_context(|| format!("command returned unsupported exit code {code}"))?;
            Ok(ExitCode::from(code))
        }
    }
}
