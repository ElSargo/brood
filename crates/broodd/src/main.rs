use std::process::ExitCode;

use anyhow::Context;

fn main() -> anyhow::Result<ExitCode> {
    let result = broodd::run_cli().and_then(|outcome| match outcome {
        broodd::CommandOutcome::Completed => Ok(ExitCode::SUCCESS),
        broodd::CommandOutcome::Exit(code) => u8::try_from(code)
            .with_context(|| format!("command returned unsupported exit code {code}"))
            .map(ExitCode::from),
    });
    brood_profiler::flush();
    result
}
