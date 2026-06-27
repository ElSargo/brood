use anyhow::{Result, bail};
use manifoldd::launch_context;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

fn main() -> Result<()> {
    let record = launch_context::current_launch_record()?;
    let Some((program, args)) = record.program_argv.split_first() else {
        bail!("Manifold launch record has empty program argv");
    };

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&record.working_dir)
        .envs(record.env_pairs());

    #[cfg(unix)]
    {
        Err(command.exec()).map_err(Into::into)
    }

    #[cfg(not(unix))]
    {
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            bail!("Manifold runner exec exited with {status}");
        }
    }
}
