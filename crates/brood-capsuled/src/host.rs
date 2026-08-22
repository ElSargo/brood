//! Host-side systemd and Bubblewrap mechanism adapter.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{
    plan::{CapsulePlan, CgroupController, MountEntry, PlanError},
    protocol::{Request, RequestFrame, Response, ResponseFrame},
    transport::{ReceivedPacket, SeqPacket, TransportError},
};

const READY_REQUEST_ID: u64 = 0;
const READINESS_POLL: Duration = Duration::from_millis(25);
const BROKER_IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostExecutables {
    pub systemd_run: PathBuf,
    pub systemctl: PathBuf,
    pub bubblewrap: PathBuf,
    pub pasta: PathBuf,
    pub capsuled: PathBuf,
}

impl HostExecutables {
    pub fn discover(capsuled: PathBuf) -> Result<Self, ExecutableError> {
        let executables = Self {
            systemd_run: find_executable("systemd-run")?,
            systemctl: find_executable("systemctl")?,
            bubblewrap: find_executable("bwrap")?,
            pasta: find_executable("pasta")?,
            capsuled,
        };
        executables.validate()?;
        Ok(executables)
    }

    pub fn validate(&self) -> Result<(), ExecutableError> {
        for (name, path) in [
            ("systemd-run", &self.systemd_run),
            ("systemctl", &self.systemctl),
            ("bwrap", &self.bubblewrap),
            ("pasta", &self.pasta),
            ("brood-capsuled", &self.capsuled),
        ] {
            validate_executable(name, path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
}

impl Invocation {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        command
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPlan {
    pub bubblewrap: Invocation,
    pub service: Invocation,
    pub pasta: PlannedPasta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedPasta {
    pub service_unit: String,
    pub program: PathBuf,
    pub arguments_before_pid: Vec<OsString>,
}

impl PlannedPasta {
    #[must_use]
    pub fn invocation_for_pid(&self, pid: u32) -> Invocation {
        let mut arguments = self.arguments_before_pid.clone();
        arguments.push(pid.to_string().into());
        Invocation {
            program: self.program.clone(),
            arguments,
        }
    }
}

pub fn build_host_plan(
    plan: &CapsulePlan,
    executables: &HostExecutables,
) -> Result<HostPlan, HostBuildError> {
    plan.validate()?;
    executables.validate()?;
    let bubblewrap = bubblewrap_invocation(plan, executables)?;
    let service = service_invocation(plan, executables, &bubblewrap);
    let service_stem = plan
        .service
        .unit_name
        .strip_suffix(".service")
        .ok_or_else(|| HostBuildError::InvalidServiceName(plan.service.unit_name.clone()))?;
    let pasta = PlannedPasta {
        service_unit: format!("{service_stem}-pasta.service"),
        program: executables.pasta.clone(),
        arguments_before_pid: vec![
            "--foreground".into(),
            "--quiet".into(),
            "--config-net".into(),
            "--no-map-gw".into(),
            "--tcp-ports".into(),
            "none".into(),
            "--udp-ports".into(),
            "none".into(),
            "--tcp-ns".into(),
            "none".into(),
            "--udp-ns".into(),
            "none".into(),
        ],
    };
    Ok(HostPlan {
        bubblewrap,
        service,
        pasta,
    })
}

fn bubblewrap_invocation(
    plan: &CapsulePlan,
    executables: &HostExecutables,
) -> Result<Invocation, HostBuildError> {
    let mut arguments = vec![
        "--unshare-user".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--unshare-net".into(),
        "--unshare-cgroup".into(),
        "--as-pid-1".into(),
        "--new-session".into(),
    ];
    for entry in &plan.mounts.entries {
        append_mount(entry, &mut arguments);
    }

    let runtime_directory = plan
        .broker_socket
        .parent()
        .ok_or_else(|| HostBuildError::BrokerSocketWithoutParent(plan.broker_socket.clone()))?;
    if plan
        .mounts
        .entries
        .iter()
        .any(|entry| entry.target() == runtime_directory)
    {
        return Err(HostBuildError::BrokerRuntimeMountConflict(
            runtime_directory.to_path_buf(),
        ));
    }
    arguments.extend([
        "--bind".into(),
        runtime_directory.as_os_str().to_owned(),
        runtime_directory.as_os_str().to_owned(),
        "--".into(),
        executables.capsuled.as_os_str().to_owned(),
        "inner".into(),
        "--broker-socket".into(),
        plan.broker_socket.as_os_str().to_owned(),
        "--termination-grace-ms".into(),
        duration_millis(plan.service.termination_grace)
            .to_string()
            .into(),
    ]);
    Ok(Invocation {
        program: executables.bubblewrap.clone(),
        arguments,
    })
}

fn append_mount(entry: &MountEntry, arguments: &mut Vec<OsString>) {
    match entry {
        MountEntry::ReadOnlyBind { source, target } => {
            append_paths(arguments, "--ro-bind", [source.as_path(), target.as_path()]);
        }
        MountEntry::WritableBind { source, target } => {
            append_paths(arguments, "--bind", [source.as_path(), target.as_path()]);
        }
        MountEntry::Overlay(overlay) => {
            for lower in &overlay.lowers {
                append_paths(arguments, "--overlay-src", [lower.as_path()]);
            }
            append_paths(
                arguments,
                "--overlay",
                [
                    overlay.upper.as_path(),
                    overlay.work.as_path(),
                    overlay.target.as_path(),
                ],
            );
        }
        MountEntry::Tmpfs { target } | MountEntry::Hide { target } => {
            append_paths(arguments, "--tmpfs", [target.as_path()]);
        }
        MountEntry::Proc { target } => {
            append_paths(arguments, "--proc", [target.as_path()]);
        }
        MountEntry::Device { source, target } => append_paths(
            arguments,
            "--dev-bind",
            [source.as_path(), target.as_path()],
        ),
    }
}

fn append_paths<const N: usize>(
    arguments: &mut Vec<OsString>,
    option: &'static str,
    paths: [&Path; N],
) {
    arguments.push(option.into());
    arguments.extend(paths.into_iter().map(Path::as_os_str).map(OsStr::to_owned));
}

fn service_invocation(
    plan: &CapsulePlan,
    executables: &HostExecutables,
    bubblewrap: &Invocation,
) -> Invocation {
    let delegated = plan
        .service
        .delegate
        .iter()
        .copied()
        .map(CgroupController::systemd_name)
        .collect::<Vec<_>>()
        .join(" ");
    let properties = [
        "Type=exec".to_owned(),
        format!("Slice={}", plan.service.slice),
        format!("Delegate={delegated}"),
        format!("CPUAccounting={}", yes_no(plan.service.accounting.cpu)),
        format!(
            "MemoryAccounting={}",
            yes_no(plan.service.accounting.memory)
        ),
        format!("IOAccounting={}", yes_no(plan.service.accounting.io)),
        format!("TasksAccounting={}", yes_no(plan.service.accounting.tasks)),
        "KillMode=control-group".to_owned(),
        "SendSIGKILL=yes".to_owned(),
        format!(
            "TimeoutStopSec={}ms",
            duration_millis(plan.service.termination_grace)
        ),
    ];
    let mut arguments = vec![
        "--user".into(),
        "--quiet".into(),
        "--no-block".into(),
        "--collect".into(),
        format!("--unit={}", plan.service.unit_name).into(),
    ];
    for property in properties {
        arguments.push("--property".into());
        arguments.push(property.into());
    }
    arguments.push("--".into());
    arguments.push(bubblewrap.program.as_os_str().to_owned());
    arguments.extend(bubblewrap.arguments.iter().cloned());
    Invocation {
        program: executables.systemd_run.clone(),
        arguments,
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

const fn duration_millis(duration: Duration) -> u128 {
    duration.as_millis()
}

/// A ready service and the connected daemon-to-broker channel.
#[derive(Debug)]
pub struct PreparedCapsule {
    service_unit: String,
    broker_socket: PathBuf,
    broker: Option<SeqPacket>,
    executables: HostExecutables,
    pasta: PlannedPasta,
    stopped: bool,
}

impl PreparedCapsule {
    #[must_use]
    pub const fn broker(&self) -> Option<&SeqPacket> {
        self.broker.as_ref()
    }

    #[must_use]
    pub const fn pasta_plan(&self) -> &PlannedPasta {
        &self.pasta
    }

    pub fn shutdown(mut self) -> Result<(), CleanupError> {
        self.shutdown_inner()?;
        self.stopped = true;
        Ok(())
    }

    fn shutdown_inner(&mut self) -> Result<(), CleanupError> {
        if let Some(broker) = self.broker.take() {
            broker.set_receive_timeout(Some(Duration::from_secs(2)))?;
            broker.send(&RequestFrame::current(1, Request::Shutdown), &[])?;
            let response: ReceivedPacket<ResponseFrame> = broker.receive()?;
            if response.message.request_id != 1
                || response.message.body != Response::ShuttingDown
                || !response.descriptors.is_empty()
            {
                return Err(CleanupError::Protocol(response.message));
            }
        }
        wait_for_inactive(
            &self.executables,
            &self.service_unit,
            Duration::from_secs(2),
        )?;
        remove_owned_socket(&self.broker_socket)?;
        reset_failed(&self.executables, &self.service_unit);
        Ok(())
    }
}

impl Drop for PreparedCapsule {
    fn drop(&mut self) {
        if !self.stopped {
            let _cleanup = self.shutdown_inner().or_else(|_error| {
                stop_service(&self.executables, &self.service_unit)?;
                remove_owned_socket(&self.broker_socket)
            });
        }
    }
}

pub fn prepare_isolated(
    plan: &CapsulePlan,
    executables: &HostExecutables,
    readiness_timeout: Duration,
) -> Result<PreparedCapsule, PrepareError> {
    let host_plan = build_host_plan(plan, executables)?;
    if readiness_timeout.is_zero() {
        return Err(PrepareError::ZeroReadinessTimeout);
    }
    if plan.broker_socket.exists() {
        return Err(PrepareError::BrokerSocketExists(plan.broker_socket.clone()));
    }
    let runtime_directory = plan
        .broker_socket
        .parent()
        .ok_or_else(|| PrepareError::RuntimeDirectory(plan.broker_socket.clone()))?;
    let metadata =
        fs::metadata(runtime_directory).map_err(|source| PrepareError::RuntimeDirectoryIo {
            path: runtime_directory.to_path_buf(),
            source,
        })?;
    if !metadata.is_dir() {
        return Err(PrepareError::RuntimeDirectory(
            runtime_directory.to_path_buf(),
        ));
    }
    let expected_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected_uid || metadata.permissions().mode() & 0o077 != 0 {
        return Err(PrepareError::SharedRuntimeDirectory {
            path: runtime_directory.to_path_buf(),
            expected_uid,
            actual_uid: metadata.uid(),
            mode: metadata.permissions().mode() & 0o777,
        });
    }

    let initial_state = service_state(executables, &plan.service.unit_name)?;
    if initial_state.load != "not-found" {
        return Err(PrepareError::ServiceAlreadyExists {
            unit: plan.service.unit_name.clone(),
            state: Box::new(initial_state),
        });
    }

    let output = host_plan.service.command().output()?;
    if !output.status.success() {
        return Err(PrepareError::StartService(Box::new(command_failure(
            &output,
        ))));
    }

    let broker = match wait_for_readiness(
        plan,
        executables,
        readiness_timeout,
        &plan.service.unit_name,
    ) {
        Ok(broker) => broker,
        Err(error) => {
            let _cleanup = stop_service(executables, &plan.service.unit_name);
            let _cleanup = remove_owned_socket(&plan.broker_socket);
            reset_failed(executables, &plan.service.unit_name);
            return Err(error);
        }
    };

    Ok(PreparedCapsule {
        service_unit: plan.service.unit_name.clone(),
        broker_socket: plan.broker_socket.clone(),
        broker: Some(broker),
        executables: executables.clone(),
        pasta: host_plan.pasta,
        stopped: false,
    })
}

fn wait_for_readiness(
    plan: &CapsulePlan,
    executables: &HostExecutables,
    timeout: Duration,
    service_unit: &str,
) -> Result<SeqPacket, PrepareError> {
    let deadline = Instant::now() + timeout;
    loop {
        match SeqPacket::connect_path(&plan.broker_socket) {
            Ok(broker) => {
                broker.set_receive_timeout(Some(BROKER_IO_TIMEOUT))?;
                broker.set_send_timeout(Some(BROKER_IO_TIMEOUT))?;
                broker.send(&RequestFrame::current(READY_REQUEST_ID, Request::Ping), &[])?;
                let response: ReceivedPacket<ResponseFrame> = broker.receive()?;
                if response.message.request_id != READY_REQUEST_ID
                    || response.message.body != Response::Pong
                    || response.message.version != crate::protocol::PROTOCOL_VERSION
                    || !response.descriptors.is_empty()
                {
                    return Err(PrepareError::ReadinessProtocol(response.message));
                }
                broker.set_receive_timeout(None)?;
                return Ok(broker);
            }
            Err(TransportError::Io(rustix::io::Errno::NOENT | rustix::io::Errno::CONNREFUSED)) => {}
            Err(error) => return Err(error.into()),
        }

        let last_state = service_state(executables, service_unit)?;
        if last_state.failed() || last_state.inactive_after_start() {
            return Err(PrepareError::ServiceFailed {
                unit: service_unit.to_owned(),
                state: Box::new(last_state),
            });
        }
        if Instant::now() >= deadline {
            return Err(PrepareError::ReadinessTimeout {
                socket: plan.broker_socket.clone(),
                timeout,
                state: Box::new(last_state),
            });
        }
        thread::sleep(READINESS_POLL);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceState {
    pub load: String,
    pub active: String,
    pub sub: String,
    pub result: String,
    pub main_pid: u32,
    pub exec_main_code: i32,
    pub exec_main_status: i32,
}

impl ServiceState {
    fn failed(&self) -> bool {
        self.active == "failed" || (self.result != "success" && !self.result.is_empty())
    }

    fn inactive_after_start(&self) -> bool {
        self.load != "not-found" && self.active == "inactive"
    }
}

fn service_state(executables: &HostExecutables, unit: &str) -> Result<ServiceState, PrepareError> {
    let output = Command::new(&executables.systemctl)
        .args([
            "--user",
            "show",
            unit,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=MainPID",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
        ])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut state = ServiceState::default();
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "LoadState" => value.clone_into(&mut state.load),
            "ActiveState" => value.clone_into(&mut state.active),
            "SubState" => value.clone_into(&mut state.sub),
            "Result" => value.clone_into(&mut state.result),
            "MainPID" => state.main_pid = value.parse().map_or(0, |parsed| parsed),
            "ExecMainCode" => state.exec_main_code = value.parse().map_or(0, |parsed| parsed),
            "ExecMainStatus" => {
                state.exec_main_status = value.parse().map_or(0, |parsed| parsed);
            }
            _ => {}
        }
    }
    if state.load.is_empty() {
        return Err(PrepareError::InspectService(Box::new(command_failure(
            &output,
        ))));
    }
    Ok(state)
}

fn wait_for_inactive(
    executables: &HostExecutables,
    unit: &str,
    timeout: Duration,
) -> Result<(), CleanupError> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = service_state(executables, unit)?;
        if state.load == "not-found" || state.active == "inactive" || state.active == "failed" {
            return Ok(());
        }
        if Instant::now() >= deadline {
            stop_service(executables, unit)?;
            return Ok(());
        }
        thread::sleep(READINESS_POLL);
    }
}

fn stop_service(executables: &HostExecutables, unit: &str) -> Result<(), CleanupError> {
    let output = Command::new(&executables.systemctl)
        .args(["--user", "stop", unit])
        .output()?;
    if output.status.success() || service_state(executables, unit)?.load == "not-found" {
        Ok(())
    } else {
        Err(CleanupError::StopService(command_failure(&output)))
    }
}

fn reset_failed(executables: &HostExecutables, unit: &str) {
    let _output = Command::new(&executables.systemctl)
        .args(["--user", "reset-failed", unit])
        .output();
}

fn remove_owned_socket(path: &Path) -> Result<(), CleanupError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CleanupError::RemoveSocket {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn find_executable(name: &'static str) -> Result<PathBuf, ExecutableError> {
    let path = env::var_os("PATH").ok_or(ExecutableError::MissingPath)?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if validate_executable(name, &candidate).is_ok() {
            return candidate
                .canonicalize()
                .map_err(|source| ExecutableError::Unavailable {
                    name,
                    path: candidate,
                    source,
                });
        }
    }
    Err(ExecutableError::NotFound { name })
}

fn validate_executable(name: &'static str, path: &Path) -> Result<(), ExecutableError> {
    if !path.is_absolute() {
        return Err(ExecutableError::Relative {
            name,
            path: path.to_path_buf(),
        });
    }
    let metadata = fs::metadata(path).map_err(|source| ExecutableError::Unavailable {
        name,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ExecutableError::NotExecutable {
            name,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandFailure {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn command_failure(output: &Output) -> CommandFailure {
    CommandFailure {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[derive(Debug, Error)]
pub enum ExecutableError {
    #[error("PATH is absent while discovering capsule host executables")]
    MissingPath,
    #[error("required capsule executable {name} was not found in PATH")]
    NotFound { name: &'static str },
    #[error("required capsule executable {name} path is not absolute: {path:?}")]
    Relative { name: &'static str, path: PathBuf },
    #[error("required capsule executable {name} at {path:?} is unavailable: {source}")]
    Unavailable {
        name: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("required capsule executable {name} at {path:?} is not an executable file")]
    NotExecutable { name: &'static str, path: PathBuf },
}

#[derive(Debug, Error)]
pub enum HostBuildError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Executable(#[from] ExecutableError),
    #[error("broker socket has no private runtime parent: {0:?}")]
    BrokerSocketWithoutParent(PathBuf),
    #[error("broker runtime directory conflicts with an explicit mount target: {0:?}")]
    BrokerRuntimeMountConflict(PathBuf),
    #[error("invalid service name: {0}")]
    InvalidServiceName(String),
}

#[derive(Debug, Error)]
pub enum PrepareError {
    #[error("capsule host plan is invalid: {0}")]
    Build(#[from] HostBuildError),
    #[error("readiness timeout must be nonzero")]
    ZeroReadinessTimeout,
    #[error("broker socket already exists: {0:?}")]
    BrokerSocketExists(PathBuf),
    #[error("broker socket does not have a private runtime directory: {0:?}")]
    RuntimeDirectory(PathBuf),
    #[error("cannot inspect broker runtime directory {path:?}: {source}")]
    RuntimeDirectoryIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "broker runtime directory {path:?} is not private: expected uid {expected_uid}, actual uid {actual_uid}, mode {mode:o}"
    )]
    SharedRuntimeDirectory {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
        mode: u32,
    },
    #[error("capsule service {unit} already exists in state {state:?}")]
    ServiceAlreadyExists {
        unit: String,
        state: Box<ServiceState>,
    },
    #[error("systemd rejected the transient capsule service: {0:?}")]
    StartService(Box<CommandFailure>),
    #[error("could not inspect capsule service: {0:?}")]
    InspectService(Box<CommandFailure>),
    #[error("capsule service {unit} failed before readiness: {state:?}")]
    ServiceFailed {
        unit: String,
        state: Box<ServiceState>,
    },
    #[error("capsule broker at {socket:?} was not ready within {timeout:?}: {state:?}")]
    ReadinessTimeout {
        socket: PathBuf,
        timeout: Duration,
        state: Box<ServiceState>,
    },
    #[error("capsule readiness handshake returned an invalid response: {0:?}")]
    ReadinessProtocol(ResponseFrame),
    #[error("capsule readiness transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("capsule host command failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum CleanupError {
    #[error("capsule broker shutdown transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("capsule broker returned an invalid shutdown response: {0:?}")]
    Protocol(ResponseFrame),
    #[error("cannot inspect capsule service during cleanup: {0}")]
    Inspect(#[source] Box<PrepareError>),
    #[error("systemd could not stop capsule service: {0:?}")]
    StopService(CommandFailure),
    #[error("capsule cleanup command failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not remove owned broker socket {path:?}: {source}")]
    RemoveSocket {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl From<PrepareError> for CleanupError {
    fn from(error: PrepareError) -> Self {
        Self::Inspect(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::plan::{AccountingPlan, MountPlan, NamespacePlan, ServicePlan};

    fn executables() -> HostExecutables {
        let executable = PathBuf::from("/proc/self/exe");
        HostExecutables {
            systemd_run: executable.clone(),
            systemctl: executable.clone(),
            bubblewrap: executable.clone(),
            pasta: executable.clone(),
            capsuled: executable,
        }
    }

    fn plan() -> CapsulePlan {
        CapsulePlan {
            namespaces: NamespacePlan::standard_isolated(),
            mounts: MountPlan {
                entries: vec![
                    MountEntry::ReadOnlyBind {
                        source: PathBuf::from("/"),
                        target: PathBuf::from("/"),
                    },
                    MountEntry::Proc {
                        target: PathBuf::from("/proc"),
                    },
                    MountEntry::Tmpfs {
                        target: PathBuf::from("/tmp"),
                    },
                    MountEntry::Device {
                        source: PathBuf::from("/dev"),
                        target: PathBuf::from("/dev"),
                    },
                ],
            },
            service: ServicePlan {
                unit_name: "brood-capsule-test.service".into(),
                slice: "brood-sessions.slice".into(),
                delegate: BTreeSet::from([
                    CgroupController::Cpu,
                    CgroupController::Cpuset,
                    CgroupController::Io,
                    CgroupController::Memory,
                    CgroupController::Pids,
                ]),
                accounting: AccountingPlan {
                    cpu: true,
                    memory: true,
                    io: true,
                    tasks: true,
                },
                termination_grace: Duration::from_secs(3),
            },
            broker_socket: PathBuf::from("/run/user/1000/brood/private/broker.sock"),
        }
    }

    #[test]
    fn builder_has_fail_closed_namespaces_mounts_and_service_policy() {
        let built = build_host_plan(&plan(), &executables());
        assert!(built.is_ok());
        let Ok(built) = built else {
            return;
        };
        for required in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
            "--unshare-cgroup",
            "--as-pid-1",
            "--ro-bind",
            "--proc",
            "--tmpfs",
            "--dev-bind",
            "--bind",
        ] {
            assert!(built.bubblewrap.arguments.contains(&required.into()));
        }
        for required in [
            "--collect",
            "Delegate=cpu cpuset io memory pids",
            "CPUAccounting=yes",
            "MemoryAccounting=yes",
            "IOAccounting=yes",
            "TasksAccounting=yes",
            "KillMode=control-group",
        ] {
            assert!(built.service.arguments.contains(&required.into()));
        }
        for required in [
            "--no-map-gw",
            "--tcp-ports",
            "--udp-ports",
            "--tcp-ns",
            "--udp-ns",
        ] {
            assert!(built.pasta.arguments_before_pid.contains(&required.into()));
        }
    }

    #[test]
    fn preparation_rejects_shared_runtime_directory_before_starting_service() {
        let runtime = tempfile::tempdir();
        assert!(runtime.is_ok());
        let Ok(runtime) = runtime else {
            return;
        };
        let permissions = fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o755));
        assert!(permissions.is_ok());
        let mut plan = plan();
        plan.broker_socket = runtime.path().join("broker.sock");
        let prepared = prepare_isolated(&plan, &executables(), Duration::from_secs(1));
        assert!(matches!(
            prepared,
            Err(PrepareError::SharedRuntimeDirectory { .. })
        ));
    }
}
