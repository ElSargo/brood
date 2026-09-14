use std::{env, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};
use brood_cli::{NestedTestArgs, SshTestArgs, TestArgs, TestCommand};

pub fn run(args: TestArgs) -> Result<()> {
    match args
        .command
        .unwrap_or_else(|| TestCommand::Nested(NestedTestArgs::default()))
    {
        TestCommand::Themes(args) => run_themes(args),
        TestCommand::Packages(args) => run_packages(args),
        TestCommand::Knowledge(args) => run_knowledge(args),
        TestCommand::ComputerUse(args) => run_computer_use(args),
        TestCommand::ComputerUseSpeedcrunch(args) => run_computer_use_speedcrunch(args),
        TestCommand::ComputerUseAgent(args) => run_computer_use_agent(args),
        TestCommand::CodingAgent(args) => run_coding_agent(args),
        TestCommand::Nested(args) => run_nested(args),
        TestCommand::Chromium(args) => run_chromium(args),
        TestCommand::PointerReconnect(args) => run_pointer_reconnect(args),
        TestCommand::TerminalReconnect(args) => run_terminal_reconnect(args),
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
    run_agent_with(args, false)
}

fn run_coding_agent(args: NestedTestArgs) -> Result<()> {
    run_agent_with(args, true)
}

fn run_agent_with(args: NestedTestArgs, offline: bool) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-coding-agent-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if offline {
        command.env("BROOD_CODING_AGENT_OFFLINE", "1");
    } else {
        command.env("BROOD_CODING_AGENT_COMPUTER_USE", "1");
    }
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

fn run_chromium(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-chromium-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_CHROMIUM_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("Chromium nested proof exited with {status}");
    }
    Ok(())
}

fn run_terminal_reconnect(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-session-switcher-plugin-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    command.env("BROOD_TERMINAL_RECONNECT_ONLY", "1");
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_SESSION_SWITCHER_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("terminal reconnect proof exited with {status}");
    }
    Ok(())
}

fn run_pointer_reconnect(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let script = root.join("tools/test-session-switcher-plugin-nested.sh");
    let mut command = Command::new("bash");
    command.arg(&script).current_dir(&root);
    command.env("BROOD_POINTER_RECONNECT_ONLY", "1");
    if let Some(out_dir) = args.out_dir {
        command.env("BROOD_SESSION_SWITCHER_OUT", out_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("run {}", script.display()))?;
    if !status.success() {
        bail!("pointer reconnect proof exited with {status}");
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

fn run_knowledge(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let output = args
        .out_dir
        .unwrap_or_else(|| root.join("target/knowledge-proof"));
    let status = Command::new("python3")
        .arg(root.join("tools/test-knowledge-offline.py"))
        .arg("--binary")
        .arg(env::current_exe()?)
        .arg("--out-dir")
        .arg(output)
        .status()
        .context("run offline knowledge proof")?;
    if !status.success() {
        bail!("offline knowledge proof exited with {status}");
    }
    Ok(())
}

fn run_packages(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let binary = env::current_exe()?;
    let sibling_client = binary
        .parent()
        .context("Brood executable has no parent")?
        .join("brood-client");
    // Nix splits native binaries into separate outputs. Its wrapper puts the
    // assembled package on PATH, while current_exe resolves the backend output.
    let client = if sibling_client.is_file() {
        sibling_client
    } else {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .map(|path| path.join("brood-client"))
            .find(|path| path.is_file())
            .context("locate brood-client beside Brood or on PATH")?
    };
    let output = args
        .out_dir
        .unwrap_or_else(|| root.join("target/package-proof"));
    let status = Command::new("python3")
        .arg(root.join("tools/test-self-describing-packages.py"))
        .arg("--binary")
        .arg(binary)
        .arg("--client")
        .arg(client)
        .arg("--root")
        .arg(root)
        .arg("--out-dir")
        .arg(output)
        .status()
        .context("run package lifecycle proof")?;
    if !status.success() {
        bail!("package lifecycle proof exited with {status}");
    }
    Ok(())
}

fn run_themes(args: NestedTestArgs) -> Result<()> {
    let root = find_checkout_root()?;
    let mut command = Command::new("bash");
    command
        .arg(root.join("tools/test-session-switcher-plugin-nested.sh"))
        .current_dir(&root)
        .env("BROOD_THEMES_ONLY", "1")
        .env(
            "RUST_LOG",
            "broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn",
        )
        .env("BROOD_EMPTY_STARTUP_ONLY", "1")
        .env(
            "BROOD_TEST_FACET_RENDER_COMPLETION_DELAY",
            "session-switcher=120",
        )
        .env("BROOD_TEST_FACET_DELAY_ALL_RENDERS", "1");
    if let Some(out) = args.out_dir {
        command.env("BROOD_SESSION_SWITCHER_OUT", out);
    }
    let status = command.status().context("run offscreen theme proof")?;
    if !status.success() {
        bail!("theme proof exited with {status}");
    }
    Ok(())
}
