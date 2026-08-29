use std::{env, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use brood_cli::{NestedTestArgs, SshTestArgs, TestArgs, TestCommand};

pub fn run(args: TestArgs) -> Result<()> {
    match args
        .command
        .unwrap_or_else(|| TestCommand::Nested(NestedTestArgs::default()))
    {
        TestCommand::ComputerUse(args) => run_computer_use(args),
        TestCommand::ComputerUseSpeedcrunch(args) => run_computer_use_speedcrunch(args),
        TestCommand::ComputerUseAgent(args) => run_computer_use_agent(args),
        TestCommand::Nested(args) => run_nested(args),
        TestCommand::RenderScheduling(args) => run_render_scheduling(args),
        TestCommand::Editor(args) => run_editor(args),
        TestCommand::Resize(args) => run_resize(args),
        TestCommand::Ssh(args) => run_ssh(args),
    }
}

fn run_render_scheduling(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-render-scheduling-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_RENDER_SCHEDULING_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("render-scheduling nested proof exited with {status}");
    }
    Ok(())
}

fn run_computer_use_agent(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-coding-agent-nested.sh");
    let mut command = Command::new("bash");
    command
        .arg(&script)
        .current_dir(&root)
        .env("BROOD_CODING_AGENT_COMPUTER_USE", "1");
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_CODING_AGENT_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("built-in computer-use agent proof exited with {status}");
    }
    Ok(())
}

fn run_computer_use_speedcrunch(args: NestedTestArgs) -> Result<()> {
    run_computer_use_with(args, true)
}

fn run_computer_use(args: NestedTestArgs) -> Result<()> {
    run_computer_use_with(args, false)
}

fn run_computer_use_with(args: NestedTestArgs, speedcrunch: bool) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-computer-use-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if speedcrunch {
        command.env("BROOD_COMPUTER_USE_SPEEDCRUNCH", "1");
    }
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_COMPUTER_USE_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("computer-use nested proof exited with {status}");
    }
    Ok(())
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

fn run_editor(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-editor-plugin-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_EDITOR_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("nested editor proof exited with {status}");
    }
    Ok(())
}

fn run_resize(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-resize-overhaul-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_RESIZE_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("nested resize proof exited with {status}");
    }
    Ok(())
}

fn run_ssh(args: SshTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-ssh-multi-backend.sh");
    let mut command = Command::new("bash");
    command.arg(&script).arg(&args.target).current_dir(&root);
    if let Some(connect_host) = args.connect_host {
        command.arg("--connect-host").arg(connect_host);
    }
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_SSH_MULTI_BACKEND_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("SSH multi-backend proof exited with {status}");
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
        .context("run `brood test` from a Brood source checkout")
}
