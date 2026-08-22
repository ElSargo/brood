//! Pure plans for later systemd, namespace, and mount mechanism adapters.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapsulePlan {
    pub namespaces: NamespacePlan,
    pub mounts: MountPlan,
    pub service: ServicePlan,
    pub broker_socket: PathBuf,
}

impl CapsulePlan {
    pub fn validate(&self) -> Result<(), PlanError> {
        self.namespaces.validate()?;
        self.mounts.validate()?;
        self.service.validate()?;
        require_absolute("broker socket", &self.broker_socket)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespacePlan {
    pub user: bool,
    pub mount: bool,
    pub pid: bool,
    pub ipc: bool,
    pub uts: bool,
    pub network: bool,
    pub cgroup: bool,
    /// Preserve support for nested rootless containers.
    pub allow_nested_user_namespaces: bool,
}

impl NamespacePlan {
    #[must_use]
    pub const fn standard_isolated() -> Self {
        Self {
            user: true,
            mount: true,
            pid: true,
            ipc: true,
            uts: true,
            network: true,
            cgroup: true,
            allow_nested_user_namespaces: true,
        }
    }

    pub fn validate(&self) -> Result<(), PlanError> {
        let required = [
            ("user", self.user),
            ("mount", self.mount),
            ("pid", self.pid),
            ("ipc", self.ipc),
            ("uts", self.uts),
            ("network", self.network),
            ("cgroup", self.cgroup),
        ];
        for (namespace, enabled) in required {
            if !enabled {
                return Err(PlanError::MissingRequiredNamespace { namespace });
            }
        }
        if !self.allow_nested_user_namespaces {
            return Err(PlanError::NestedUserNamespacesDisabled);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountPlan {
    pub entries: Vec<MountEntry>,
}

impl MountPlan {
    pub fn validate(&self) -> Result<(), PlanError> {
        let mut targets = BTreeSet::new();
        for entry in &self.entries {
            let target = entry.target();
            require_absolute("mount target", target)?;
            if !targets.insert(target.to_path_buf()) {
                return Err(PlanError::DuplicateMountTarget(target.to_path_buf()));
            }
            entry.validate()?;
        }
        if !self.entries.iter().any(|entry| {
            entry.target() == Path::new("/")
                && matches!(
                    entry,
                    MountEntry::ReadOnlyBind { .. } | MountEntry::Overlay(_)
                )
        }) {
            return Err(PlanError::MissingRootMount);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountEntry {
    ReadOnlyBind { source: PathBuf, target: PathBuf },
    WritableBind { source: PathBuf, target: PathBuf },
    Overlay(OverlayMount),
    Tmpfs { target: PathBuf },
    Proc { target: PathBuf },
    Device { source: PathBuf, target: PathBuf },
    Hide { target: PathBuf },
}

impl MountEntry {
    #[must_use]
    pub fn target(&self) -> &Path {
        match self {
            Self::ReadOnlyBind { target, .. }
            | Self::WritableBind { target, .. }
            | Self::Tmpfs { target }
            | Self::Proc { target }
            | Self::Device { target, .. }
            | Self::Hide { target } => target,
            Self::Overlay(overlay) => &overlay.target,
        }
    }

    fn validate(&self) -> Result<(), PlanError> {
        match self {
            Self::ReadOnlyBind { source, .. }
            | Self::WritableBind { source, .. }
            | Self::Device { source, .. } => require_absolute("mount source", source),
            Self::Overlay(overlay) => overlay.validate(),
            Self::Tmpfs { .. } | Self::Proc { .. } | Self::Hide { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayMount {
    pub lowers: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub target: PathBuf,
}

impl OverlayMount {
    fn validate(&self) -> Result<(), PlanError> {
        if self.lowers.is_empty() {
            return Err(PlanError::OverlayWithoutLower {
                target: self.target.clone(),
            });
        }
        require_absolute("overlay upper", &self.upper)?;
        require_absolute("overlay work", &self.work)?;
        if self.upper.starts_with(&self.work) || self.work.starts_with(&self.upper) {
            return Err(PlanError::OverlayWritableDirectoriesOverlap {
                upper: self.upper.clone(),
                work: self.work.clone(),
            });
        }
        for lower in &self.lowers {
            require_absolute("overlay lower", lower)?;
            for (kind, writable) in [("upper", &self.upper), ("work", &self.work)] {
                if writable.starts_with(lower) {
                    return Err(PlanError::OverlayWritableInsideLower {
                        kind,
                        writable: writable.clone(),
                        lower: lower.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePlan {
    pub unit_name: String,
    pub slice: String,
    pub delegate: BTreeSet<CgroupController>,
    pub accounting: AccountingPlan,
    pub termination_grace: Duration,
}

impl ServicePlan {
    pub fn validate(&self) -> Result<(), PlanError> {
        if !valid_unit_component(&self.unit_name)
            || !matches!(self.unit_name.rsplit_once('.'), Some((_, "service")))
        {
            return Err(PlanError::InvalidSystemdUnit(self.unit_name.clone()));
        }
        if !valid_unit_component(&self.slice)
            || !matches!(self.slice.rsplit_once('.'), Some((_, "slice")))
        {
            return Err(PlanError::InvalidSystemdSlice(self.slice.clone()));
        }
        if self.termination_grace.is_zero() {
            return Err(PlanError::ZeroTerminationGrace);
        }
        let accounting = [
            ("CPU", self.accounting.cpu),
            ("memory", self.accounting.memory),
            ("I/O", self.accounting.io),
            ("task", self.accounting.tasks),
        ];
        for (resource, enabled) in accounting {
            if !enabled {
                return Err(PlanError::MissingAccounting { resource });
            }
        }
        for controller in [
            CgroupController::Cpu,
            CgroupController::Cpuset,
            CgroupController::Io,
            CgroupController::Memory,
            CgroupController::Pids,
        ] {
            if !self.delegate.contains(&controller) {
                return Err(PlanError::MissingDelegatedController { controller });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountingPlan {
    pub cpu: bool,
    pub memory: bool,
    pub io: bool,
    pub tasks: bool,
}

#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq)]
pub enum CgroupController {
    Cpu,
    Cpuset,
    Io,
    Memory,
    Pids,
}

impl CgroupController {
    #[must_use]
    pub const fn systemd_name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cpuset => "cpuset",
            Self::Io => "io",
            Self::Memory => "memory",
            Self::Pids => "pids",
        }
    }
}

fn require_absolute(field: &'static str, path: &Path) -> Result<(), PlanError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PlanError::RelativePath {
            field,
            path: path.to_path_buf(),
        })
    }
}

fn valid_unit_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("required {namespace} namespace is disabled")]
    MissingRequiredNamespace { namespace: &'static str },
    #[error("nested user namespaces must remain available")]
    NestedUserNamespacesDisabled,
    #[error("{field} path must be absolute: {path:?}")]
    RelativePath { field: &'static str, path: PathBuf },
    #[error("mount target is duplicated: {0:?}")]
    DuplicateMountTarget(PathBuf),
    #[error("mount plan must explicitly provide a read-only or overlay root")]
    MissingRootMount,
    #[error("overlay for {target:?} has no lower directories")]
    OverlayWithoutLower { target: PathBuf },
    #[error("overlay {kind} {writable:?} must not be inside lower {lower:?}")]
    OverlayWritableInsideLower {
        kind: &'static str,
        writable: PathBuf,
        lower: PathBuf,
    },
    #[error("overlay upper {upper:?} and work {work:?} must be separate, non-nested directories")]
    OverlayWritableDirectoriesOverlap { upper: PathBuf, work: PathBuf },
    #[error("invalid systemd service unit name: {0}")]
    InvalidSystemdUnit(String),
    #[error("invalid systemd slice name: {0}")]
    InvalidSystemdSlice(String),
    #[error("termination grace must be nonzero")]
    ZeroTerminationGrace,
    #[error("{resource} accounting must be enabled")]
    MissingAccounting { resource: &'static str },
    #[error("cgroup controller {controller:?} must be delegated")]
    MissingDelegatedController { controller: CgroupController },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> ServicePlan {
        ServicePlan {
            unit_name: "brood-capsule-volume.service".into(),
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
        }
    }

    #[test]
    fn overlay_writable_directory_under_lower_is_rejected() {
        let plan = MountPlan {
            entries: vec![
                MountEntry::ReadOnlyBind {
                    source: PathBuf::from("/"),
                    target: PathBuf::from("/"),
                },
                MountEntry::Overlay(OverlayMount {
                    lowers: vec![PathBuf::from("/home/user")],
                    upper: PathBuf::from("/home/user/.brood/upper"),
                    work: PathBuf::from("/volumes/work"),
                    target: PathBuf::from("/home/user"),
                }),
            ],
        };
        assert!(matches!(
            plan.validate(),
            Err(PlanError::OverlayWritableInsideLower { kind: "upper", .. })
        ));
    }

    #[test]
    fn overlay_upper_and_work_must_not_overlap() {
        let plan = MountPlan {
            entries: vec![
                MountEntry::ReadOnlyBind {
                    source: PathBuf::from("/"),
                    target: PathBuf::from("/"),
                },
                MountEntry::Overlay(OverlayMount {
                    lowers: vec![PathBuf::from("/lower")],
                    upper: PathBuf::from("/volume/writable"),
                    work: PathBuf::from("/volume/writable/work"),
                    target: PathBuf::from("/target"),
                }),
            ],
        };
        assert!(matches!(
            plan.validate(),
            Err(PlanError::OverlayWritableDirectoriesOverlap { .. })
        ));
    }

    #[test]
    fn valid_plan_preserves_required_isolation_policy() {
        let plan = CapsulePlan {
            namespaces: NamespacePlan::standard_isolated(),
            mounts: MountPlan {
                entries: vec![MountEntry::ReadOnlyBind {
                    source: PathBuf::from("/"),
                    target: PathBuf::from("/"),
                }],
            },
            service: service(),
            broker_socket: PathBuf::from("/run/user/1000/brood/capsule.sock"),
        };
        assert!(plan.validate().is_ok());
    }
}
