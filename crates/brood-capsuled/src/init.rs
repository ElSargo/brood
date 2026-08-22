//! Child ownership used by the capsule PID-1 broker and the explicit direct broker.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::{ffi::OsStringExt, process::CommandExt},
    },
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, WaitStatus, getpid, kill_process_group, pidfd_open, wait,
    waitpid,
};
use thiserror::Error;

use crate::protocol::{ProcessExit, ProcessSignal, SpawnRequest, StdioSource};

const MAX_TRACKED_PROCESSES: usize = 4_096;
const MAX_RETAINED_EXITS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitMode {
    CapsulePidOne,
    Direct,
}

/// A successfully spawned process and its stable kernel identity.
#[derive(Debug)]
pub struct SpawnedProcess {
    pub process_id: u64,
    pub pidfd: OwnedFd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitResult {
    Running,
    Exited(ProcessExit),
}

#[derive(Clone, Copy, Debug)]
struct ChildRecord {
    pid: Pid,
    process_id: u64,
}

/// Owns direct children, records exits, and terminates complete child process groups.
#[derive(Debug)]
pub struct InitProcess {
    mode: InitMode,
    next_process_id: u64,
    children: BTreeMap<i32, ChildRecord>,
    exited: BTreeMap<u64, ProcessExit>,
    exited_order: VecDeque<u64>,
}

impl InitProcess {
    pub fn new(mode: InitMode) -> Result<Self, InitError> {
        if mode == InitMode::CapsulePidOne && getpid() != Pid::INIT {
            return Err(InitError::NotPidOne { actual: getpid() });
        }
        Ok(Self {
            mode,
            next_process_id: 1,
            children: BTreeMap::new(),
            exited: BTreeMap::new(),
            exited_order: VecDeque::new(),
        })
    }

    pub fn spawn(
        &mut self,
        request: &SpawnRequest,
        descriptors: &[OwnedFd],
    ) -> Result<SpawnedProcess, InitError> {
        if self.children.len() >= MAX_TRACKED_PROCESSES {
            return Err(InitError::TooManyProcesses {
                maximum: MAX_TRACKED_PROCESSES,
            });
        }
        let process_id = self.next_process_id;
        let next_process_id = self
            .next_process_id
            .checked_add(1)
            .ok_or(InitError::ProcessIdExhausted)?;
        let mut command = Command::new(OsString::from_vec(request.program.clone()));
        command.args(request.arguments.iter().cloned().map(OsString::from_vec));
        if let Some(cwd) = &request.cwd {
            command.current_dir(OsString::from_vec(cwd.clone()));
        }
        if request.environment.clear {
            command.env_clear();
        }
        for value in &request.environment.set {
            command.env(
                OsString::from_vec(value.name.clone()),
                OsString::from_vec(value.value.clone()),
            );
        }
        for name in &request.environment.remove {
            command.env_remove(OsString::from_vec(name.clone()));
        }
        command
            .stdin(resolve_stdio(request.stdin, descriptors)?)
            .stdout(resolve_stdio(request.stdout, descriptors)?)
            .stderr(resolve_stdio(request.stderr, descriptors)?)
            .process_group(0);

        let mut child = command.spawn()?;
        let pid = Pid::from_child(&child);
        let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                let _ignored = kill_process_group(pid, Signal::KILL);
                let _ignored = child.wait();
                return Err(InitError::Pidfd { pid, error });
            }
        };
        drop(child);

        self.next_process_id = next_process_id;
        self.children
            .insert(pid.as_raw_pid(), ChildRecord { pid, process_id });
        Ok(SpawnedProcess { process_id, pidfd })
    }

    pub fn signal(&mut self, process_id: u64, signal: ProcessSignal) -> Result<(), InitError> {
        self.reap_nonblocking()?;
        let child = self.child(process_id)?;
        kill_process_group(child.pid, rustix_signal(signal))?;
        Ok(())
    }

    pub fn wait(&mut self, process_id: u64, nohang: bool) -> Result<WaitResult, InitError> {
        self.reap_nonblocking()?;
        if let Some(status) = self.exited.get(&process_id) {
            return Ok(WaitResult::Exited(*status));
        }
        let child = self.child(process_id)?;
        if nohang {
            return Ok(WaitResult::Running);
        }
        let (_, status) = waitpid(Some(child.pid), WaitOptions::empty())?
            .ok_or(InitError::MissingWaitStatus { pid: child.pid })?;
        self.record_exit(child.pid, status);
        let status = self
            .exited
            .get(&process_id)
            .copied()
            .ok_or(InitError::MissingRecordedExit { process_id })?;
        Ok(WaitResult::Exited(status))
    }

    /// Reap every exited child without blocking.
    pub fn reap_nonblocking(&mut self) -> Result<usize, InitError> {
        if self.mode == InitMode::CapsulePidOne {
            return self.reap_any_nonblocking();
        }

        let pids: Vec<Pid> = self.children.values().map(|child| child.pid).collect();
        let mut count = 0;
        for pid in pids {
            if let Some((reaped, status)) = waitpid(Some(pid), WaitOptions::NOHANG)? {
                self.record_exit(reaped, status);
                count += 1;
            }
        }
        Ok(count)
    }

    /// Forward graceful termination, then kill and reap anything that remains.
    pub fn terminate_all(&mut self, grace: Duration) -> Result<(), InitError> {
        if self.mode == InitMode::CapsulePidOne {
            return self.terminate_capsule_children(grace);
        }
        self.reap_nonblocking()?;
        for child in self.children.values() {
            match kill_process_group(child.pid, Signal::TERM) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }

        let deadline = Instant::now() + grace;
        while !self.children.is_empty() && Instant::now() < deadline {
            self.reap_nonblocking()?;
            if !self.children.is_empty() {
                thread::sleep(Duration::from_millis(10));
            }
        }

        for child in self.children.values() {
            match kill_process_group(child.pid, Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let pids: Vec<Pid> = self.children.values().map(|child| child.pid).collect();
        for pid in pids {
            match waitpid(Some(pid), WaitOptions::empty()) {
                Ok(Some((reaped, status))) => self.record_exit(reaped, status),
                Ok(None) | Err(rustix::io::Errno::CHILD) => {
                    self.children.remove(&pid.as_raw_pid());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    fn reap_any_nonblocking(&mut self) -> Result<usize, InitError> {
        let mut count = 0;
        loop {
            match wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    self.record_exit(pid, status);
                    count += 1;
                }
                Ok(None) | Err(rustix::io::Errno::CHILD) => return Ok(count),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn terminate_capsule_children(&mut self, grace: Duration) -> Result<(), InitError> {
        match signal_all_namespace_processes(Signal::TERM) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }

        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !self.capsule_children_after_reap()? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }

        match signal_all_namespace_processes(Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
        loop {
            match wait(WaitOptions::empty()) {
                Ok(Some((pid, status))) => self.record_exit(pid, status),
                Ok(None) | Err(rustix::io::Errno::CHILD) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn capsule_children_after_reap(&mut self) -> Result<bool, InitError> {
        loop {
            match wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => self.record_exit(pid, status),
                Ok(None) => return Ok(true),
                Err(rustix::io::Errno::CHILD) => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn child(&self, process_id: u64) -> Result<ChildRecord, InitError> {
        self.children
            .values()
            .find(|child| child.process_id == process_id)
            .copied()
            .ok_or(InitError::UnknownProcess { process_id })
    }

    fn record_exit(&mut self, pid: Pid, status: WaitStatus) {
        if let Some(child) = self.children.remove(&pid.as_raw_pid()) {
            self.exited.insert(child.process_id, process_exit(status));
            self.exited_order.push_back(child.process_id);
            if self.exited_order.len() > MAX_RETAINED_EXITS
                && let Some(expired) = self.exited_order.pop_front()
            {
                self.exited.remove(&expired);
            }
        }
    }
}

/// `kill_process_group(Pid::INIT, signal)` lowers to `kill(-1, signal)`.
///
/// PID 1 is a deliberate rustix sentinel here: Linux broadcasts `kill(-1)` to
/// every process the caller may signal except PID 1 itself. This reaches child
/// process groups and adopted descendants without terminating the capsule init.
fn signal_all_namespace_processes(signal: Signal) -> Result<(), rustix::io::Errno> {
    kill_process_group(Pid::INIT, signal)
}

fn resolve_stdio(source: StdioSource, descriptors: &[OwnedFd]) -> Result<Stdio, InitError> {
    match source {
        StdioSource::Inherit => Ok(Stdio::inherit()),
        StdioSource::Null => Ok(Stdio::null()),
        StdioSource::Descriptor(index) => {
            let descriptor = descriptors
                .get(usize::from(index))
                .ok_or(InitError::MissingDescriptor { index })?;
            Ok(Stdio::from(descriptor.as_fd().try_clone_to_owned()?))
        }
    }
}

const fn rustix_signal(signal: ProcessSignal) -> Signal {
    match signal {
        ProcessSignal::Hangup => Signal::HUP,
        ProcessSignal::Interrupt => Signal::INT,
        ProcessSignal::Kill => Signal::KILL,
        ProcessSignal::Terminate => Signal::TERM,
        ProcessSignal::User1 => Signal::USR1,
        ProcessSignal::User2 => Signal::USR2,
    }
}

fn process_exit(status: WaitStatus) -> ProcessExit {
    status.exit_status().map_or_else(
        || {
            status
                .terminating_signal()
                .map_or_else(|| ProcessExit::Other(status.as_raw()), ProcessExit::Signal)
        },
        ProcessExit::Code,
    )
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("capsule inner mode must run as PID 1, but its PID is {actual}")]
    NotPidOne { actual: Pid },
    #[error("process spawn failed: {0}")]
    Spawn(#[from] io::Error),
    #[error("could not open pidfd for child {pid}: {error}")]
    Pidfd { pid: Pid, error: rustix::io::Errno },
    #[error("logical process IDs are exhausted")]
    ProcessIdExhausted,
    #[error("broker already owns the maximum of {maximum} concurrent processes")]
    TooManyProcesses { maximum: usize },
    #[error("process {process_id} is not owned by this broker")]
    UnknownProcess { process_id: u64 },
    #[error("descriptor index {index} is absent")]
    MissingDescriptor { index: u8 },
    #[error("wait returned no status for blocking child {pid}")]
    MissingWaitStatus { pid: Pid },
    #[error("wait status for process {process_id} was not recorded")]
    MissingRecordedExit { process_id: u64 },
    #[error("process operation failed: {0}")]
    Process(#[from] rustix::io::Errno),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        EnvironmentDelta, LaunchMetadata, SpawnDisposition, SpawnRequest, StdioSource,
    };

    fn shell(command: &str) -> SpawnRequest {
        SpawnRequest {
            program: b"/bin/sh".to_vec(),
            arguments: vec![b"-c".to_vec(), command.as_bytes().to_vec()],
            cwd: None,
            environment: EnvironmentDelta::default(),
            stdin: StdioSource::Null,
            stdout: StdioSource::Null,
            stderr: StdioSource::Null,
            disposition: SpawnDisposition::Attached,
            metadata: LaunchMetadata::default(),
        }
    }

    fn exact_exit_status() -> Result<(), Box<dyn std::error::Error>> {
        let mut init = InitProcess::new(InitMode::Direct)?;
        let process = init.spawn(&shell("exit 23"), &[])?;
        if init.wait(process.process_id, false)? != WaitResult::Exited(ProcessExit::Code(23)) {
            return Err("child exit status was not 23".into());
        }
        if init.child_count() != 0 {
            return Err("exited child remained registered".into());
        }
        Ok(())
    }

    #[test]
    fn direct_init_waits_for_exact_exit_status() {
        assert!(exact_exit_status().is_ok());
    }

    fn terminate_child_process_group() -> Result<(), Box<dyn std::error::Error>> {
        let mut init = InitProcess::new(InitMode::Direct)?;
        let _process = init.spawn(&shell("sleep 30"), &[])?;
        init.terminate_all(Duration::from_millis(20))?;
        if init.child_count() != 0 {
            return Err("terminated child remained registered".into());
        }
        Ok(())
    }

    #[test]
    fn direct_init_terminates_child_process_group() {
        assert!(terminate_child_process_group().is_ok());
    }
}
