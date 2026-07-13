//! Built-in nested-test orchestration.
//!
//! This module owns process topology, readiness, tracked command completion,
//! artifacts, and cleanup. It deliberately does not participate in Wayland
//! protocol wiring or the compositor's calloop startup.

use std::{
    env,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use crate::{
    MANIFOLD_SESSION_ENV, NestedTestArgs, TestArgs, TestCommand, control, runtime::cli_policy_arg,
};

pub fn run(args: TestArgs) -> Result<()> {
    match args
        .command
        .unwrap_or_else(|| TestCommand::Nested(NestedTestArgs::default()))
    {
        TestCommand::Nested(args) => NestedTestPlan::from_args(args)?.run(),
    }
}

struct NestedTestPlan {
    args: NestedTestArgs,
    artifacts: NestedArtifacts,
    current_exe: PathBuf,
    inherited_session: Option<OsString>,
    outer_target: control::ControlTarget,
    inner_target: control::ControlTarget,
}

struct NestedArtifacts {
    out_dir: PathBuf,
    outer_log: PathBuf,
    inner_log: PathBuf,
}

impl NestedTestPlan {
    fn from_args(args: NestedTestArgs) -> Result<Self> {
        let out_dir = if let Some(dir) = args.out_dir.as_ref() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            dir.clone()
        } else {
            let dir = env::temp_dir().join(format!("manifold-nested-test-{}", std::process::id()));
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            dir
        };
        let bus_address = env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        Ok(Self {
            artifacts: NestedArtifacts {
                outer_log: out_dir.join("outer.log"),
                inner_log: out_dir.join("inner.log"),
                out_dir,
            },
            current_exe: env::current_exe().context("resolve current executable")?,
            inherited_session: env::var_os(MANIFOLD_SESSION_ENV),
            outer_target: control::ControlTarget::new(
                nested_dbus_destination("outer"),
                bus_address.clone(),
            ),
            inner_target: control::ControlTarget::new(
                nested_dbus_destination("inner"),
                bus_address,
            ),
            args,
        })
    }

    fn run(self) -> Result<()> {
        match nested_test_topology(&self.args, self.inherited_session.as_deref()) {
            NestedTestTopology::OuterSupervisor => self.run_with_outer(),
            NestedTestTopology::CurrentSession => self.run_in_current_session(),
        }
    }

    fn run_with_outer(self) -> Result<()> {
        let mut outer_child = spawn_outer(OuterLaunch {
            executable: &self.current_exe,
            outer_target: &self.outer_target,
            inner_target: &self.inner_target,
            log: &self.artifacts.outer_log,
            headful: self.args.outer_headful,
            harness: self.args.harness.as_deref(),
        })?;
        let cleanup = NestedCleanup::new(&self.outer_target, Some(&self.inner_target));
        let mut process =
            SupervisedProcess::new(&mut outer_child, &self.artifacts.outer_log, "outer");

        wait_for_control_bus(&self.outer_target, &mut process)?;
        spawn_inner(SubjectLaunch {
            outer_target: Some(&self.outer_target),
            executable: &self.current_exe,
            target: &self.inner_target,
            log: &self.artifacts.inner_log,
            headless: self.args.inner_headless,
            harness: None,
            config: self.args.inner_config.as_deref(),
            wasm_plugin: self.args.inner_wasm_plugin.as_deref(),
            wasm_key_policy: cli_policy_arg(self.args.inner_wasm_key_policy.as_deref())?.as_ref(),
        })?;
        wait_for_control_bus(&self.inner_target, &mut process)?;
        if !self.args.inner_headless {
            wait_for_outer_window(&self.outer_target, &mut process)?;
        }

        let command_observation = self
            .args
            .inner_command
            .as_deref()
            .map(|command| {
                run_tracked_command(
                    &self.inner_target,
                    command,
                    CommandCompletionPolicy::from_wait_for_exit(
                        self.args.wait_for_inner_command_exit,
                    ),
                    &mut process,
                )
            })
            .transpose()?;
        let outer_dump = capture_outer_debug_dump(&self.outer_target, &self.artifacts.out_dir)?;
        print_outer_summary(&self, &outer_dump, command_observation);

        if self.args.keep_running || self.args.harness.is_some() {
            let status = outer_child.wait().context("wait for outer process")?;
            println!("outer_exit={status}");
            if !status.success() {
                bail!("outer process exited with {status}");
            }
            return Ok(());
        }

        cleanup.stop_all();
        let status = outer_child.wait().context("wait for outer process")?;
        if !status.success() {
            bail!("outer process exited with {status}");
        }
        Ok(())
    }

    fn run_in_current_session(self) -> Result<()> {
        let mut subject_child = spawn_subject(SubjectLaunch {
            outer_target: None,
            executable: &self.current_exe,
            target: &self.inner_target,
            log: &self.artifacts.inner_log,
            headless: self.args.inner_headless,
            harness: self.args.harness.as_deref(),
            config: self.args.inner_config.as_deref(),
            wasm_plugin: self.args.inner_wasm_plugin.as_deref(),
            wasm_key_policy: cli_policy_arg(self.args.inner_wasm_key_policy.as_deref())?.as_ref(),
        })?;
        let cleanup = NestedCleanup::new(&self.inner_target, None);
        let mut process =
            SupervisedProcess::new(&mut subject_child, &self.artifacts.inner_log, "subject");
        wait_for_control_bus(&self.inner_target, &mut process)?;
        let command_observation = self
            .args
            .inner_command
            .as_deref()
            .map(|command| {
                run_tracked_command(
                    &self.inner_target,
                    command,
                    CommandCompletionPolicy::from_wait_for_exit(
                        self.args.wait_for_inner_command_exit,
                    ),
                    &mut process,
                )
            })
            .transpose()?;
        print_current_session_summary(&self, command_observation);

        if self.args.keep_running || self.args.harness.is_some() {
            let status = subject_child.wait().context("wait for subject process")?;
            println!("subject_exit={status}");
            if !status.success() {
                bail!("subject process exited with {status}");
            }
            return Ok(());
        }
        cleanup.stop_all();
        let status = subject_child.wait().context("wait for subject process")?;
        if !status.success() {
            bail!("subject process exited with {status}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedTestTopology {
    OuterSupervisor,
    CurrentSession,
}

fn nested_test_topology(
    args: &NestedTestArgs,
    inherited_session: Option<&OsStr>,
) -> NestedTestTopology {
    let inherited = inherited_session.is_some_and(|session| !session.is_empty());
    if args.force_outer || !inherited {
        NestedTestTopology::OuterSupervisor
    } else {
        NestedTestTopology::CurrentSession
    }
}

#[derive(Clone, Copy)]
struct OuterLaunch<'a> {
    executable: &'a Path,
    outer_target: &'a control::ControlTarget,
    inner_target: &'a control::ControlTarget,
    log: &'a Path,
    headful: bool,
    harness: Option<&'a Path>,
}

#[derive(Clone, Copy)]
struct SubjectLaunch<'a> {
    outer_target: Option<&'a control::ControlTarget>,
    executable: &'a Path,
    target: &'a control::ControlTarget,
    log: &'a Path,
    headless: bool,
    harness: Option<&'a Path>,
    config: Option<&'a Path>,
    wasm_plugin: Option<&'a Path>,
    wasm_key_policy: Option<&'a (String, PathBuf)>,
}

fn spawn_outer(launch: OuterLaunch<'_>) -> Result<Child> {
    let mut command = Command::new(launch.executable);
    command
        .args(["run", "--no-session-runner", "--no-default-modules"])
        .arg("--dbus-destination")
        .arg(&launch.outer_target.destination)
        .arg("--nested-inner-dbus-destination")
        .arg(&launch.inner_target.destination)
        .arg("--test-surface-markers");
    if let Some(address) = launch.outer_target.address.as_deref() {
        command.arg("--dbus-address").arg(address);
    }
    if let Some(address) = launch.inner_target.address.as_deref() {
        command.arg("--nested-inner-dbus-address").arg(address);
    }
    if !launch.headful {
        command.arg("--offscreen-presentation");
    }
    if let Some(harness) = launch.harness {
        command.arg("--test-harness").arg(harness);
    }
    redirect_output(&mut command, launch.log)?;
    command
        .spawn()
        .with_context(|| format!("spawn outer process {}", launch.executable.display()))
}

fn spawn_subject(launch: SubjectLaunch<'_>) -> Result<Child> {
    let mut command = subject_command(&launch);
    redirect_output(&mut command, launch.log)?;
    command
        .spawn()
        .with_context(|| format!("spawn subject process {}", launch.executable.display()))
}

fn spawn_inner(launch: SubjectLaunch<'_>) -> Result<()> {
    let outer_target = launch
        .outer_target
        .context("inner launch requires an outer control target")?;
    let mut command = format_subject_shell_command(&launch);
    write!(
        &mut command,
        " >{} 2>&1",
        shell_quote(&launch.log.display().to_string())
    )
    .context("format inner process output redirection")?;
    let response = control::request_dbus_command(
        outer_target,
        &["spawn", "bash", "-lc", &command].map(str::to_string),
        false,
    )?;
    if !response.starts_with("OK ") {
        bail!("spawn inner failed: {}", response.trim());
    }
    Ok(())
}

fn subject_command(launch: &SubjectLaunch<'_>) -> Command {
    let mut command = Command::new(launch.executable);
    command.args(["run", "--no-session-runner"]);
    add_subject_arguments(&mut command, launch);
    command
}

fn add_subject_arguments(command: &mut Command, launch: &SubjectLaunch<'_>) {
    if let Some(config) = launch.config {
        command.arg("--config").arg(config);
    } else {
        command.arg("--no-default-modules");
    }
    command
        .arg("--dbus-destination")
        .arg(&launch.target.destination)
        .arg("--test-surface-markers");
    if let Some(address) = launch.target.address.as_deref() {
        command.arg("--dbus-address").arg(address);
    }
    if launch.headless {
        command.arg("--headless");
    }
    if let Some(harness) = launch.harness {
        command.arg("--test-harness").arg(harness);
    }
    if let Some(path) = launch.wasm_plugin {
        command.arg("--wasm-plugin").arg(path);
    }
    if let Some((provider, path)) = launch.wasm_key_policy {
        command.arg("--wasm-key-policy").arg(provider).arg(path);
    }
}

fn format_subject_shell_command(launch: &SubjectLaunch<'_>) -> String {
    let mut command = format!(
        "exec {} run --no-session-runner",
        shell_quote(&launch.executable.display().to_string())
    );
    let mut push_arg = |flag: &str, value: Option<String>| {
        command.push(' ');
        command.push_str(flag);
        if let Some(value) = value {
            command.push(' ');
            command.push_str(&shell_quote(&value));
        }
    };
    if let Some(config) = launch.config {
        push_arg("--config", Some(config.display().to_string()));
    } else {
        push_arg("--no-default-modules", None);
    }
    push_arg(
        "--dbus-destination",
        Some(launch.target.destination.clone()),
    );
    if let Some(address) = launch.target.address.as_deref() {
        push_arg("--dbus-address", Some(address.to_string()));
    }
    if launch.headless {
        push_arg("--headless", None);
    }
    push_arg("--test-surface-markers", None);
    if let Some(path) = launch.wasm_plugin {
        push_arg("--wasm-plugin", Some(path.display().to_string()));
    }
    if let Some((provider, path)) = launch.wasm_key_policy {
        push_arg("--wasm-key-policy", Some(provider.clone()));
        command.push(' ');
        command.push_str(&shell_quote(&path.display().to_string()));
    }
    command
}

fn redirect_output(command: &mut Command, log: &Path) -> Result<()> {
    command.stdout(Stdio::from(
        fs::File::create(log).with_context(|| format!("create {}", log.display()))?,
    ));
    command.stderr(Stdio::from(
        fs::OpenOptions::new()
            .append(true)
            .open(log)
            .with_context(|| format!("open {}", log.display()))?,
    ));
    Ok(())
}

struct SupervisedProcess<'a> {
    child: &'a mut Child,
    log: &'a Path,
    label: &'static str,
}

impl<'a> SupervisedProcess<'a> {
    const fn new(child: &'a mut Child, log: &'a Path, label: &'static str) -> Self {
        Self { child, log, label }
    }

    fn ensure_running(&mut self, phase: &str) -> Result<()> {
        if let Some(status) = self.child.try_wait().context("poll supervised process")? {
            bail!(
                "{} exited while {phase}: {status}\n{}",
                self.label,
                read_log_tail(self.log)
            );
        }
        Ok(())
    }
}

fn wait_for_control_bus(
    target: &control::ControlTarget,
    process: &mut SupervisedProcess<'_>,
) -> Result<()> {
    wait_until(Duration::from_secs(15), || {
        process.ensure_running("waiting for the control service")?;
        Ok(
            control::request_dbus_command(target, &["status-json".to_string()], false)
                .is_ok_and(|response| response.trim_start().starts_with('{')),
        )
    })
    .with_context(|| {
        format!(
            "timed out waiting for {} DBus control service",
            process.label
        )
    })
}

fn wait_for_outer_window(
    target: &control::ControlTarget,
    process: &mut SupervisedProcess<'_>,
) -> Result<()> {
    wait_until(Duration::from_secs(15), || {
        process.ensure_running("waiting for the inner presentation to map")?;
        Ok(status_json(target).is_ok_and(|json| json_u64(&json, "windows") >= 1))
    })
    .context("timed out waiting for outer to host inner presentation")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommandObservation {
    pane_id: u64,
    visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandCompletionPolicy {
    Visible,
    Exited,
}

impl CommandCompletionPolicy {
    const fn from_wait_for_exit(wait_for_exit: bool) -> Self {
        if wait_for_exit {
            Self::Exited
        } else {
            Self::Visible
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackedCommandState {
    Launching,
    Running,
    Exited { success: bool, detail: String },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedCommandStatus {
    state: TrackedCommandState,
    visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandWaitDecision {
    Continue,
    Complete,
    Fail(String),
}

fn command_wait_decision(
    completion: CommandCompletionPolicy,
    status: &TrackedCommandStatus,
    presented: bool,
) -> CommandWaitDecision {
    match &status.state {
        TrackedCommandState::Launching | TrackedCommandState::Running => {
            if completion == CommandCompletionPolicy::Visible && presented {
                CommandWaitDecision::Complete
            } else {
                CommandWaitDecision::Continue
            }
        }
        TrackedCommandState::Exited { success: true, .. } => CommandWaitDecision::Complete,
        TrackedCommandState::Exited {
            success: false,
            detail,
        } => CommandWaitDecision::Fail(detail.clone()),
        TrackedCommandState::Unknown => {
            CommandWaitDecision::Fail("disappeared without an outcome".to_string())
        }
    }
}

fn run_tracked_command(
    target: &control::ControlTarget,
    command: &str,
    completion: CommandCompletionPolicy,
    process: &mut SupervisedProcess<'_>,
) -> Result<CommandObservation> {
    let argv = shell_split(command)?;
    let response =
        control::request_dbus_command(target, &[vec!["spawn".to_string()], argv].concat(), false)?;
    let pane_id = parse_spawned_pane_id(&response)?;
    let mut visible = false;
    wait_until(Duration::from_mins(1), || {
        process.ensure_running("waiting for the tracked inner command")?;
        let status = tracked_command_status(target, pane_id)?;
        let presented =
            status.visible && status_json(target).is_ok_and(|json| json_u64(&json, "frames") >= 2);
        visible |= presented;
        match command_wait_decision(completion, &status, presented) {
            CommandWaitDecision::Continue => Ok(false),
            CommandWaitDecision::Complete => Ok(true),
            CommandWaitDecision::Fail(detail) => {
                bail!("tracked inner command pane {pane_id} failed: {detail}")
            }
        }
    })
    .context("timed out waiting for tracked inner command completion")?;
    Ok(CommandObservation { pane_id, visible })
}

fn tracked_command_status(
    target: &control::ControlTarget,
    pane_id: u64,
) -> Result<TrackedCommandStatus> {
    let response = control::request_dbus_command(
        target,
        &["hosted-child-status".to_string(), pane_id.to_string()],
        false,
    )?;
    let json: serde_json::Value = serde_json::from_str(&response)?;
    let state = match json.get("state").and_then(serde_json::Value::as_str) {
        Some("launching") => TrackedCommandState::Launching,
        Some("running") => TrackedCommandState::Running,
        Some("exited") => TrackedCommandState::Exited {
            success: json
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            detail: json
                .get("detail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing exit detail")
                .to_string(),
        },
        _ => TrackedCommandState::Unknown,
    };
    Ok(TrackedCommandStatus {
        state,
        visible: json
            .get("visible")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_spawned_pane_id(response: &str) -> Result<u64> {
    response
        .split_whitespace()
        .find_map(|field| field.strip_prefix("pane_id="))
        .context("spawn response did not identify its hosted child")?
        .parse()
        .context("spawn response contained an invalid pane id")
}

fn status_json(target: &control::ControlTarget) -> Result<serde_json::Value> {
    let response = control::request_dbus_command(target, &["status-json".to_string()], false)?;
    serde_json::from_str(&response).context("parse nested status JSON")
}

fn json_u64(json: &serde_json::Value, key: &str) -> u64 {
    json.get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn wait_until(timeout: Duration, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let started = Instant::now();
    loop {
        if ready()? {
            return Ok(());
        }
        if started.elapsed() > timeout {
            bail!("deadline elapsed");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn capture_outer_debug_dump(target: &control::ControlTarget, out_dir: &Path) -> Result<PathBuf> {
    let dump_dir = out_dir.join("outer-dump");
    fs::create_dir_all(&dump_dir).with_context(|| format!("create {}", dump_dir.display()))?;
    let response = control::request_dbus_command(
        target,
        &[
            "debug".to_string(),
            "dump".to_string(),
            dump_dir.display().to_string(),
            "state,composed,surfaces".to_string(),
        ],
        false,
    )?;
    if !response.starts_with("OK ") {
        bail!("outer debug dump failed: {}", response.trim());
    }
    Ok(dump_dir)
}

fn print_outer_summary(
    plan: &NestedTestPlan,
    outer_dump: &Path,
    command: Option<CommandObservation>,
) {
    println!("OK nested test ready");
    println!("out_dir={}", plan.artifacts.out_dir.display());
    println!("outer_dbus_destination={}", plan.outer_target.destination);
    println!("inner_dbus_destination={}", plan.inner_target.destination);
    println!("outer_log={}", plan.artifacts.outer_log.display());
    println!("inner_log={}", plan.artifacts.inner_log.display());
    println!("outer_dump={}", outer_dump.display());
    println!("topology=outer-supervisor");
    println!(
        "outer_presentation={}",
        if plan.args.outer_headful {
            "winit"
        } else {
            "offscreen"
        }
    );
    println!("outer_headless=false");
    println!("inner_headless={}", plan.args.inner_headless);
    print_command_observation(command);
}

fn print_current_session_summary(plan: &NestedTestPlan, command: Option<CommandObservation>) {
    println!("OK nested test ready");
    println!("out_dir={}", plan.artifacts.out_dir.display());
    println!("topology=current-session");
    println!("subject_dbus_destination={}", plan.inner_target.destination);
    println!("subject_log={}", plan.artifacts.inner_log.display());
    println!("inner_dbus_destination={}", plan.inner_target.destination);
    println!("inner_log={}", plan.artifacts.inner_log.display());
    println!("inner_headless={}", plan.args.inner_headless);
    if let Some(session) = plan.inherited_session.as_ref() {
        println!(
            "inherited_{MANIFOLD_SESSION_ENV}={}",
            session.to_string_lossy()
        );
    }
    print_command_observation(command);
}

fn print_command_observation(command: Option<CommandObservation>) {
    if let Some(command) = command {
        println!("inner_command_pane_id={}", command.pane_id);
        println!("inner_command_visible={}", command.visible);
        println!("inner_command_completed=true");
    }
}

fn nested_dbus_destination(label: &str) -> String {
    format!("org.manifold.Test.p{}.{}", std::process::id(), label)
}

fn shell_split(command: &str) -> Result<Vec<String>> {
    let command = command.trim();
    if command.is_empty() {
        bail!("inner command must not be empty");
    }
    Ok(command
        .split_whitespace()
        .map(ToString::to_string)
        .collect())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn read_log_tail(path: &Path) -> String {
    fs::read_to_string(path).map_or_else(
        |err| format!("log unreadable: {err}"),
        |contents| {
            let skip = contents.lines().count().saturating_sub(40);
            contents.lines().skip(skip).collect::<Vec<_>>().join("\n")
        },
    )
}

struct NestedCleanup {
    primary: control::ControlTarget,
    secondary: Option<control::ControlTarget>,
}

impl NestedCleanup {
    fn new(primary: &control::ControlTarget, secondary: Option<&control::ControlTarget>) -> Self {
        Self {
            primary: primary.clone(),
            secondary: secondary.cloned(),
        }
    }

    fn stop_all(&self) {
        if let Some(target) = self.secondary.as_ref() {
            drop(control::request_dbus_command(
                target,
                &["quit".to_string()],
                false,
            ));
        }
        drop(control::request_dbus_command(
            &self.primary,
            &["quit".to_string()],
            false,
        ));
    }
}

impl Drop for NestedCleanup {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{
        CommandCompletionPolicy, CommandWaitDecision, NestedTestTopology, TrackedCommandState,
        TrackedCommandStatus, command_wait_decision, nested_test_topology, parse_spawned_pane_id,
    };
    use crate::NestedTestArgs;

    #[test]
    fn nested_test_uses_outer_without_session_marker() {
        assert_eq!(
            nested_test_topology(&NestedTestArgs::default(), None),
            NestedTestTopology::OuterSupervisor
        );
    }

    #[test]
    fn nested_test_uses_current_session_with_session_marker() {
        assert_eq!(
            nested_test_topology(
                &NestedTestArgs::default(),
                Some(OsStr::new("dbus-destination=org.manifold.Session"))
            ),
            NestedTestTopology::CurrentSession
        );
    }

    #[test]
    fn force_outer_overrides_session_marker() {
        let args = NestedTestArgs {
            force_outer: true,
            ..NestedTestArgs::default()
        };
        assert_eq!(
            nested_test_topology(
                &args,
                Some(OsStr::new("dbus-destination=org.manifold.Session"))
            ),
            NestedTestTopology::OuterSupervisor
        );
    }

    #[test]
    fn spawn_response_selects_exact_child_identity() {
        assert_eq!(
            parse_spawned_pane_id("OK spawned weston-terminal pane_id=42\n").ok(),
            Some(42)
        );
        assert!(parse_spawned_pane_id("OK spawned weston-terminal\n").is_err());
    }

    #[test]
    fn visible_policy_requires_the_tracked_pane_to_be_presented() {
        let running = TrackedCommandStatus {
            state: TrackedCommandState::Running,
            visible: false,
        };
        assert_eq!(
            command_wait_decision(CommandCompletionPolicy::Visible, &running, false),
            CommandWaitDecision::Continue
        );
        assert_eq!(
            command_wait_decision(CommandCompletionPolicy::Visible, &running, true),
            CommandWaitDecision::Complete
        );
    }

    #[test]
    fn exit_policy_does_not_treat_a_visible_pane_as_completion() {
        let running = TrackedCommandStatus {
            state: TrackedCommandState::Running,
            visible: true,
        };
        assert_eq!(
            command_wait_decision(CommandCompletionPolicy::Exited, &running, true),
            CommandWaitDecision::Continue
        );
    }

    #[test]
    fn tracked_failure_is_never_hidden_by_visibility() {
        let failed = TrackedCommandStatus {
            state: TrackedCommandState::Exited {
                success: false,
                detail: "exit:7".to_string(),
            },
            visible: true,
        };
        assert_eq!(
            command_wait_decision(CommandCompletionPolicy::Visible, &failed, true),
            CommandWaitDecision::Fail("exit:7".to_string())
        );
    }
}
