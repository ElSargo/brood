use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use brood_capsuled::{
    host::{HostBuildError, HostExecutables, PrepareError, prepare_isolated},
    plan::{
        AccountingPlan, CapsulePlan, CgroupController, MountEntry, MountPlan, NamespacePlan,
        ServicePlan,
    },
    protocol::{
        EnvironmentDelta, LaunchMetadata, ProcessExit, Request, RequestFrame, Response,
        ResponseFrame, SpawnDisposition, SpawnRequest, StdioSource,
    },
    transport::ReceivedPacket,
};

struct HostCleanup {
    systemctl: PathBuf,
    service_unit: String,
    marker: PathBuf,
}

impl Drop for HostCleanup {
    fn drop(&mut self) {
        let _output = Command::new(&self.systemctl)
            .args(["--user", "stop", &self.service_unit])
            .output();
        let _output = Command::new(&self.systemctl)
            .args(["--user", "reset-failed", &self.service_unit])
            .output();
        let _remove = fs::remove_file(&self.marker);
    }
}

fn isolated_plan(unit_name: String, broker_socket: PathBuf) -> CapsulePlan {
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
            unit_name,
            slice: "brood-sessions.slice".to_owned(),
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
            termination_grace: Duration::from_secs(2),
        },
        broker_socket,
    }
}

fn spawn_probe(
    broker: &brood_capsuled::transport::SeqPacket,
    marker: &Path,
    probe_path: &Path,
) -> Result<String, Box<dyn Error>> {
    let shell = format!(
        "printf 'pid=%s\\ninit=%s\\nmnt=%s\\ntmp_before=%s\\n' \\
         \"$$\" \"$(cat /proc/1/comm)\" \"$(readlink /proc/self/ns/mnt)\" \\
         \"$(if test -e '{}'; then echo present; else echo absent; fi)\" > '{}'; \\
         printf capsule > '{}'",
        marker.display(),
        probe_path.display(),
        marker.display()
    );
    broker.send(
        &RequestFrame::current(
            10,
            Request::Spawn(SpawnRequest {
                program: b"/bin/sh".to_vec(),
                arguments: vec![b"-c".to_vec(), shell.into_bytes()],
                cwd: None,
                environment: EnvironmentDelta::default(),
                stdin: StdioSource::Null,
                stdout: StdioSource::Null,
                stderr: StdioSource::Null,
                disposition: SpawnDisposition::Attached,
                metadata: LaunchMetadata::default(),
            }),
        ),
        &[],
    )?;
    let spawned: ReceivedPacket<ResponseFrame> = broker.receive()?;
    let process_id = match spawned.message.body {
        Response::Spawned {
            process_id,
            pidfd: 0,
        } if spawned.descriptors.len() == 1 => process_id,
        response => return Err(format!("unexpected spawn response: {response:?}").into()),
    };

    broker.send(
        &RequestFrame::current(
            11,
            Request::Wait {
                process_id,
                nohang: false,
            },
        ),
        &[],
    )?;
    let waited: ReceivedPacket<ResponseFrame> = broker.receive()?;
    if waited.message.body
        != (Response::Exited {
            process_id,
            status: ProcessExit::Code(0),
        })
    {
        return Err(format!("unexpected wait response: {:?}", waited.message.body).into());
    }
    Ok(fs::read_to_string(probe_path)?)
}

fn run_host_smoke() -> Result<(), Box<dyn Error>> {
    let runtime = tempfile::Builder::new()
        .prefix("brood-capsule-smoke-")
        .tempdir()?;
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
    let unique = format!("{}", std::process::id());
    let unit = format!("brood-capsule-smoke-{unique}.service");
    let socket = runtime.path().join("broker.sock");
    let probe_path = runtime.path().join("probe.txt");
    let marker = PathBuf::from(format!("/tmp/brood-capsule-smoke-{unique}.marker"));
    fs::write(&marker, b"host")?;

    let mut executables =
        HostExecutables::discover(PathBuf::from(env!("CARGO_BIN_EXE_brood-capsuled")))?;
    let _cleanup = HostCleanup {
        systemctl: executables.systemctl.clone(),
        service_unit: unit.clone(),
        marker: marker.clone(),
    };
    let plan = isolated_plan(unit, socket.clone());
    let capsule = prepare_isolated(&plan, &executables, Duration::from_secs(10))?;
    let broker = capsule.broker().ok_or("prepared capsule has no broker")?;
    let probe = spawn_probe(broker, &marker, &probe_path)?;

    if !probe.contains("pid=2\n")
        || !probe.contains("init=brood-capsuled")
        || !probe.contains("tmp_before=absent")
    {
        return Err(format!("isolation probe failed: {probe:?}").into());
    }
    let inner_mount = probe
        .lines()
        .find_map(|line| line.strip_prefix("mnt="))
        .ok_or("probe omitted mount namespace")?;
    let host_mount = fs::read_link("/proc/self/ns/mnt")?;
    if inner_mount == host_mount.to_string_lossy() {
        return Err("capsule did not receive a private mount namespace".into());
    }
    if fs::read(&marker)? != b"host" {
        return Err("capsule tmpfs write escaped into host /tmp".into());
    }

    capsule.shutdown()?;
    if socket.exists() {
        return Err("broker socket remained after clean shutdown".into());
    }

    executables.bubblewrap = PathBuf::from("/definitely/missing/bwrap");
    let failed_plan = isolated_plan(
        format!("brood-capsule-fail-closed-{unique}.service"),
        runtime.path().join("must-not-exist.sock"),
    );
    let failed = prepare_isolated(&failed_plan, &executables, Duration::from_secs(1));
    if !matches!(
        failed,
        Err(PrepareError::Build(HostBuildError::Executable(_)))
    ) {
        return Err(format!("invalid host mechanism did not fail closed: {failed:?}").into());
    }
    if failed_plan.broker_socket.exists() {
        return Err("fail-closed preparation created a broker socket".into());
    }
    Ok(())
}

#[test]
#[ignore = "requires Linux user systemd and rootless namespaces"]
fn systemd_bubblewrap_capsule_is_ready_and_isolated() {
    let result = run_host_smoke();
    assert!(result.is_ok(), "host smoke failed: {result:?}");
}
