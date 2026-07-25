use std::{env, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use brood_cli::{NestedTestArgs, TestArgs, TestCommand};

pub fn run(args: TestArgs) -> Result<()> {
    match args
        .command
        .unwrap_or_else(|| TestCommand::Nested(NestedTestArgs::default()))
    {
        TestCommand::Nested(args) => run_nested(args),
    }
}

fn run_nested(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-terminal-plugin-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_TERMINAL_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("nested terminal proof exited with {status}");
    }
    Ok(())
}

fn find_checkout_root() -> Result<PathBuf> {
    let current = env::current_dir().context("resolve current working directory")?;
    current
        .ancestors()
        .find(|candidate| {
            candidate
                .join("tools/test-terminal-plugin-nested.sh")
                .is_file()
                && candidate.join("flake.nix").is_file()
        })
        .map(PathBuf::from)
        .context("run `brood test nested` from a Brood source checkout")
}
