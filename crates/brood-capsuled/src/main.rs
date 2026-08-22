use std::{collections::BTreeSet, env, path::PathBuf, time::Duration};

use brood_capsuled::{
    broker::serve_path,
    host::{HostExecutables, prepare_isolated},
    init::InitMode,
    plan::{
        AccountingPlan, CapsulePlan, CgroupController, MountEntry, MountPlan, NamespacePlan,
        ServicePlan,
    },
};
use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(about = "Brood session capsule supervisor and spawn broker")]
struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Validate an isolated capsule request and prepare its host supervisor.
    Outer {
        #[arg(long)]
        volume_id: String,
        #[arg(long)]
        broker_socket: PathBuf,
    },
    /// Run as the PID-1 broker inside prepared namespaces.
    Inner {
        #[arg(long)]
        broker_socket: PathBuf,
        #[arg(long, default_value_t = 3_000)]
        termination_grace_ms: u64,
    },
    /// Run the broker without isolation. This mode must be selected explicitly.
    Direct {
        #[arg(long)]
        broker_socket: PathBuf,
        #[arg(long, default_value_t = 3_000)]
        termination_grace_ms: u64,
    },
}

fn main() -> Result<(), MainError> {
    match Args::parse().mode {
        Mode::Outer {
            volume_id,
            broker_socket,
        } => {
            let executables = HostExecutables::discover(env::current_exe()?)?;
            prepare_isolated(
                &scaffold_plan(&volume_id, broker_socket),
                &executables,
                Duration::from_secs(10),
            )?
            .shutdown()?;
        }
        Mode::Inner {
            broker_socket,
            termination_grace_ms,
        } => serve_path(
            &broker_socket,
            InitMode::CapsulePidOne,
            Duration::from_millis(termination_grace_ms),
        )?,
        Mode::Direct {
            broker_socket,
            termination_grace_ms,
        } => serve_path(
            &broker_socket,
            InitMode::Direct,
            Duration::from_millis(termination_grace_ms),
        )?,
    }
    Ok(())
}

fn scaffold_plan(volume_id: &str, broker_socket: PathBuf) -> CapsulePlan {
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
            unit_name: format!("brood-capsule-{volume_id}.service"),
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
            termination_grace: Duration::from_secs(3),
        },
        broker_socket,
    }
}

#[derive(Debug, Error)]
enum MainError {
    #[error(transparent)]
    Executable(#[from] brood_capsuled::host::ExecutableError),
    #[error(transparent)]
    Preparation(#[from] brood_capsuled::host::PrepareError),
    #[error(transparent)]
    Cleanup(#[from] brood_capsuled::host::CleanupError),
    #[error(transparent)]
    Broker(#[from] brood_capsuled::broker::BrokerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
