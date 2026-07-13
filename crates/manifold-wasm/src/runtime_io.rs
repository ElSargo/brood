//! Generation-owned subprocess, terminal, loopback, and timer resources.
//!
//! Resources publish readiness through [`CoordinationHub`]. They deliberately
//! do not expose a second wait mechanism: actors use bounded `try_*` methods,
//! while workers project the notification into their ordinary poll path.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::{Notify, watch},
    task::JoinHandle,
};

use crate::{CoordinationHub, NotificationError, NotificationId};

static NEXT_RESOURCE_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime authority granted to one actor generation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeIoCapabilities {
    /// PATH-resolved executable names which may be launched.
    pub commands: Vec<String>,
    /// Whether a command may own a real host pseudo-terminal.
    pub allow_pty: bool,
    /// Whether outbound connections to loopback addresses are allowed.
    pub allow_loopback: bool,
}

/// Resource and byte bounds for one actor generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeIoConfig {
    /// Granted runtime authority.
    pub capabilities: RuntimeIoCapabilities,
    /// Maximum streams, children, and timers owned at once.
    pub max_resources: usize,
    /// Maximum live child processes.
    pub max_children: usize,
    /// Maximum live timers.
    pub max_timers: usize,
    /// Read and write queue capacity for every stream direction.
    pub stream_capacity: usize,
    /// Largest payload accepted by one `try_write` call.
    pub max_write_bytes: usize,
}

impl Default for RuntimeIoConfig {
    fn default() -> Self {
        Self {
            capabilities: RuntimeIoCapabilities::default(),
            max_resources: 64,
            max_children: 8,
            max_timers: 32,
            stream_capacity: 64 * 1024,
            max_write_bytes: 64 * 1024,
        }
    }
}

/// Synchronous admission or handle failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RuntimeIoError {
    /// The owning generation has stopped or the handle is stale.
    #[error("runtime I/O generation has stopped")]
    StaleHandle,
    /// A configured resource bound was reached.
    #[error("runtime I/O resource limit reached")]
    LimitReached,
    /// The requested capability was not granted.
    #[error("runtime I/O capability not granted")]
    NotPermitted,
    /// A name, address, size, or other argument is invalid.
    #[error("invalid runtime I/O argument")]
    InvalidArgument,
    /// The host could not create the requested resource.
    #[error("host runtime I/O operation failed")]
    HostFailure,
    /// The bounded write queue cannot currently accept bytes.
    #[error("stream write would block")]
    WouldBlock,
    /// The stream write direction has closed.
    #[error("stream is closed")]
    Closed,
}

/// Result of one bounded, nonblocking read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamRead {
    /// Bytes were available immediately.
    Data(Vec<u8>),
    /// No bytes are available and EOF has not been observed.
    WouldBlock,
    /// The peer closed its write direction and every byte was drained.
    Eof,
}

/// Query-only child state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStatus {
    /// The child has not exited.
    Running,
    /// The child exited with a status code, or without one after a signal.
    Exited(Option<i32>),
}

/// Query-only timer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerStatus {
    /// The deadline has not elapsed.
    Pending,
    /// The deadline elapsed.
    Expired,
}

/// One subprocess and its three bounded pipe streams.
#[derive(Clone)]
pub struct ProcessIo {
    /// Child lifecycle handle.
    pub child: ChildHandle,
    /// Child standard input.
    pub stdin: ByteStream,
    /// Child standard output.
    pub stdout: ByteStream,
    /// Child standard error.
    pub stderr: ByteStream,
}

/// One subprocess attached to a real pseudo-terminal.
#[derive(Clone)]
pub struct PtyIo {
    /// Child lifecycle handle.
    pub child: ChildHandle,
    /// Combined terminal input and output.
    pub terminal: ByteStream,
}

/// Bounded duplex byte stream shared by pipes, PTYs, and loopback sockets.
#[derive(Clone)]
pub struct ByteStream {
    inner: Arc<StreamInner>,
}

struct StreamInner {
    id: u64,
    generation: u64,
    notification: NotificationId,
    coordination: CoordinationHub,
    capacity: usize,
    max_write_bytes: usize,
    alive: AtomicBool,
    signal_generation: AtomicU64,
    state: Mutex<StreamState>,
    changed: Condvar,
    async_changed: Notify,
    linked_child: Mutex<Option<Weak<ChildInner>>>,
}

#[derive(Debug, Default)]
struct StreamState {
    incoming: VecDeque<u8>,
    outgoing: VecDeque<u8>,
    read_eof: bool,
    write_closed: bool,
}

/// Generation-owned child handle.
#[derive(Clone)]
pub struct ChildHandle {
    inner: Arc<ChildInner>,
}

struct ChildInner {
    id: u64,
    generation: u64,
    notification: NotificationId,
    coordination: CoordinationHub,
    resolved_command: PathBuf,
    signal_generation: AtomicU64,
    alive: AtomicBool,
    shutdown: Notify,
    exited: Notify,
    status: Mutex<ChildStatus>,
}

/// Generation-owned timer handle.
#[derive(Clone)]
pub struct TimerHandle {
    inner: Arc<TimerInner>,
}

struct TimerInner {
    id: u64,
    generation: u64,
    notification: NotificationId,
    coordination: CoordinationHub,
    alive: AtomicBool,
    status: Mutex<TimerStatus>,
}

/// Shared runtime-I/O owner for one actor generation.
#[derive(Clone)]
pub struct RuntimeIoHub {
    inner: Arc<HubInner>,
}

struct HubInner {
    generation: u64,
    coordination: CoordinationHub,
    config: RuntimeIoConfig,
    allowed_commands: HashSet<String>,
    alive: AtomicBool,
    admission: Mutex<()>,
    connect_shutdown: watch::Sender<bool>,
    inflight_connects: AtomicUsize,
    inflight_changed: Notify,
    inflight_cleanups: AtomicUsize,
    cleanup_changed: Notify,
    resources: AtomicUsize,
    children_count: AtomicUsize,
    timers_count: AtomicUsize,
    streams: Mutex<HashMap<u64, Arc<StreamInner>>>,
    children: Mutex<HashMap<u64, Arc<ChildInner>>>,
    timers: Mutex<HashMap<u64, Arc<TimerInner>>>,
    tasks: Mutex<HashMap<u64, Vec<JoinHandle<()>>>>,
    #[cfg(test)]
    connect_gate: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
    #[cfg(test)]
    connect_registration_gate:
        Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    #[cfg(test)]
    cleanup_gate: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

struct ResourceReservation {
    inner: Arc<HubInner>,
    amount: usize,
    committed: bool,
}

struct ConnectGuard {
    inner: Arc<HubInner>,
}

struct CleanupGuard {
    inner: Arc<HubInner>,
}

impl CleanupGuard {
    fn new(inner: &Arc<HubInner>) -> Self {
        inner.inflight_cleanups.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: Arc::clone(inner),
        }
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        self.inner.inflight_cleanups.fetch_sub(1, Ordering::AcqRel);
        self.inner.cleanup_changed.notify_waiters();
    }
}

impl ConnectGuard {
    fn new(inner: &Arc<HubInner>) -> Self {
        inner.inflight_connects.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: Arc::clone(inner),
        }
    }
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        self.inner.inflight_connects.fetch_sub(1, Ordering::AcqRel);
        self.inner.inflight_changed.notify_waiters();
    }
}

impl ResourceReservation {
    fn new(inner: &Arc<HubInner>, amount: usize) -> Result<Self, RuntimeIoError> {
        reserve_counter(&inner.resources, amount, inner.config.max_resources)?;
        Ok(Self {
            inner: Arc::clone(inner),
            amount,
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.inner
                .resources
                .fetch_sub(self.amount, Ordering::AcqRel);
        }
    }
}

impl RuntimeIoHub {
    /// Construct a runtime owner sharing this generation's coordination hub.
    pub fn new(
        generation: u64,
        config: RuntimeIoConfig,
        coordination: CoordinationHub,
    ) -> Result<Self, RuntimeIoError> {
        if generation == 0
            || generation != coordination.generation()
            || config.max_resources == 0
            || config.max_children == 0
            || config.max_timers == 0
            || config.stream_capacity == 0
            || config.max_write_bytes == 0
        {
            return Err(RuntimeIoError::InvalidArgument);
        }
        let allowed_commands = config.capabilities.commands.iter().cloned().collect();
        let (connect_shutdown, _receiver) = watch::channel(false);
        Ok(Self {
            inner: Arc::new(HubInner {
                generation,
                coordination,
                config,
                allowed_commands,
                alive: AtomicBool::new(true),
                admission: Mutex::new(()),
                connect_shutdown,
                inflight_connects: AtomicUsize::new(0),
                inflight_changed: Notify::new(),
                inflight_cleanups: AtomicUsize::new(0),
                cleanup_changed: Notify::new(),
                resources: AtomicUsize::new(0),
                children_count: AtomicUsize::new(0),
                timers_count: AtomicUsize::new(0),
                streams: Mutex::new(HashMap::new()),
                children: Mutex::new(HashMap::new()),
                timers: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                #[cfg(test)]
                connect_gate: Mutex::new(None),
                #[cfg(test)]
                connect_registration_gate: Mutex::new(None),
                #[cfg(test)]
                cleanup_gate: Mutex::new(None),
            }),
        })
    }

    /// Spawn an allowlisted command with bounded pipe streams.
    pub fn spawn_process(
        &self,
        command: &str,
        arguments: &[String],
    ) -> Result<ProcessIo, RuntimeIoError> {
        let _admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let resolved = self.resolve_command(command)?;
        self.reserve_child()?;
        let child_state = match self.create_child(resolved.clone()) {
            Ok(state) => state,
            Err(error) => {
                self.release_child();
                return Err(error);
            }
        };
        let stdin_stream = match self.create_stream_admitted() {
            Ok(stream) => stream,
            Err(error) => {
                self.rollback_child(&child_state);
                return Err(error);
            }
        };
        let stdout_stream = match self.create_stream_admitted() {
            Ok(stream) => stream,
            Err(error) => {
                self.rollback_stream(&stdin_stream);
                self.rollback_child(&child_state);
                return Err(error);
            }
        };
        let stderr_stream = match self.create_stream_admitted() {
            Ok(stream) => stream,
            Err(error) => {
                self.rollback_stream(&stdin_stream);
                self.rollback_stream(&stdout_stream);
                self.rollback_child(&child_state);
                return Err(error);
            }
        };
        let spawned = Command::new(&resolved)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(_error) => {
                self.rollback_stream(&stdin_stream);
                self.rollback_stream(&stdout_stream);
                self.rollback_stream(&stderr_stream);
                self.rollback_child(&child_state);
                return Err(RuntimeIoError::HostFailure);
            }
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _result = child.start_kill();
            self.rollback_stream(&stdin_stream);
            self.rollback_stream(&stdout_stream);
            self.rollback_stream(&stderr_stream);
            self.rollback_child(&child_state);
            return Err(RuntimeIoError::HostFailure);
        };
        self.spawn_writer(Arc::clone(&stdin_stream.inner), stdin);
        self.spawn_reader(Arc::clone(&stdout_stream.inner), stdout);
        self.spawn_reader(Arc::clone(&stderr_stream.inner), stderr);
        let child_handle = self.monitor_std_child(child, child_state)?;
        Ok(ProcessIo {
            child: child_handle,
            stdin: stdin_stream,
            stdout: stdout_stream,
            stderr: stderr_stream,
        })
    }

    /// Spawn an allowlisted command attached to a real host pseudo-terminal.
    pub fn spawn_pty(
        &self,
        command: &str,
        arguments: &[String],
        rows: u16,
        cols: u16,
    ) -> Result<PtyIo, RuntimeIoError> {
        let _admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        if !self.inner.config.capabilities.allow_pty {
            return Err(RuntimeIoError::NotPermitted);
        }
        if rows == 0 || cols == 0 {
            return Err(RuntimeIoError::InvalidArgument);
        }
        let resolved = self.resolve_command(command)?;
        let cwd = env::current_dir().map_err(|_error| RuntimeIoError::HostFailure)?;
        self.reserve_child()?;
        let child_state = match self.create_child(resolved.clone()) {
            Ok(state) => state,
            Err(error) => {
                self.release_child();
                return Err(error);
            }
        };
        let terminal = match self.create_stream_admitted() {
            Ok(stream) => stream,
            Err(error) => {
                self.rollback_child(&child_state);
                return Err(error);
            }
        };
        let pair = match native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(_error) => {
                self.rollback_stream(&terminal);
                self.rollback_child(&child_state);
                return Err(RuntimeIoError::HostFailure);
            }
        };
        let mut builder = CommandBuilder::new(&resolved);
        // `portable-pty` otherwise falls back to `$HOME`, unlike
        // `std::process::Command`, which inherits the caller's cwd.
        builder.cwd(cwd);
        for argument in arguments {
            builder.arg(argument);
        }
        let mut child = match pair.slave.spawn_command(builder) {
            Ok(child) => child,
            Err(_error) => {
                self.rollback_stream(&terminal);
                self.rollback_child(&child_state);
                return Err(RuntimeIoError::HostFailure);
            }
        };
        drop(pair.slave);
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(_error) => {
                let _result = child.kill();
                let _result = child.wait();
                self.rollback_stream(&terminal);
                self.rollback_child(&child_state);
                return Err(RuntimeIoError::HostFailure);
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(_error) => {
                let _result = child.kill();
                let _result = child.wait();
                self.rollback_stream(&terminal);
                self.rollback_child(&child_state);
                return Err(RuntimeIoError::HostFailure);
            }
        };
        self.spawn_blocking_reader(Arc::clone(&terminal.inner), reader);
        self.spawn_blocking_writer(Arc::clone(&terminal.inner), writer);
        if let Ok(mut linked_child) = terminal.inner.linked_child.lock() {
            *linked_child = Some(Arc::downgrade(&child_state));
        }
        let child_handle = self.monitor_pty_child(child, pair.master, child_state)?;
        Ok(PtyIo {
            child: child_handle,
            terminal,
        })
    }

    /// Connect one bounded stream to a numeric loopback socket address.
    pub async fn connect_loopback(
        &self,
        address: SocketAddr,
    ) -> Result<ByteStream, RuntimeIoError> {
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        if !self.inner.config.capabilities.allow_loopback {
            return Err(RuntimeIoError::NotPermitted);
        }
        if !address.ip().is_loopback() {
            return Err(RuntimeIoError::NotPermitted);
        }
        #[cfg(test)]
        if let Some((entered, release)) = self
            .inner
            .connect_registration_gate
            .lock()
            .ok()
            .and_then(|mut gate| gate.take())
        {
            let _result = entered.send(());
            let _result = release.recv();
        }
        let _connect = ConnectGuard::new(&self.inner);
        let reservation = ResourceReservation::new(&self.inner, 1)?;
        let mut shutdown = self.inner.connect_shutdown.subscribe();
        drop(admission);
        let socket = tokio::select! {
            result = tokio::net::TcpStream::connect(address) => {
                result.map_err(|_error| RuntimeIoError::HostFailure)?
            }
            _ = shutdown.changed() => {
                return Err(RuntimeIoError::StaleHandle);
            }
        };
        #[cfg(test)]
        {
            let gate = self
                .inner
                .connect_gate
                .lock()
                .ok()
                .and_then(|gate| gate.clone());
            if let Some((entered, release)) = gate {
                entered.notify_one();
                tokio::select! {
                    () = release.notified() => {}
                    _ = shutdown.changed() => {
                        return Err(RuntimeIoError::StaleHandle);
                    }
                }
            }
        }
        let _admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let (reader, writer) = socket.into_split();
        let stream = match self.create_stream_reserved() {
            Ok(stream) => stream,
            Err(error) => return Err(error),
        };
        reservation.commit();
        self.spawn_reader(Arc::clone(&stream.inner), reader);
        self.spawn_writer(Arc::clone(&stream.inner), writer);
        Ok(stream)
    }

    /// Create a one-shot relative timer.
    pub fn create_timer(&self, duration: Duration) -> Result<TimerHandle, RuntimeIoError> {
        let _admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let deadline = tokio::time::Instant::now() + duration;
        self.reserve_timer()?;
        let id = match allocate_resource_id() {
            Ok(id) => id,
            Err(error) => {
                self.release_timer();
                return Err(error);
            }
        };
        let notification = match self.inner.coordination.create_notification() {
            Ok(notification) => notification,
            Err(error) => {
                self.release_timer();
                return Err(map_notification_error(error));
            }
        };
        let timer = Arc::new(TimerInner {
            id,
            generation: self.inner.generation,
            notification,
            coordination: self.inner.coordination.clone(),
            alive: AtomicBool::new(true),
            status: Mutex::new(TimerStatus::Pending),
        });
        self.inner
            .timers
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .insert(timer.id, Arc::clone(&timer));
        let task_timer = Arc::clone(&timer);
        self.push_task(
            timer.id,
            tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                if task_timer.alive.load(Ordering::Acquire) {
                    if let Ok(mut status) = task_timer.status.lock() {
                        *status = TimerStatus::Expired;
                    }
                    let _result = task_timer.coordination.signal(task_timer.notification, 1);
                }
            }),
        )?;
        Ok(TimerHandle { inner: timer })
    }

    /// Resolve an allowlisted command through the current PATH.
    pub fn resolve_command(&self, command: &str) -> Result<PathBuf, RuntimeIoError> {
        let mut components = Path::new(command).components();
        if command.is_empty()
            || !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || !self.inner.allowed_commands.contains(command)
        {
            return Err(RuntimeIoError::NotPermitted);
        }
        let path = env::var_os("PATH").ok_or(RuntimeIoError::HostFailure)?;
        let cwd = env::current_dir().map_err(|_error| RuntimeIoError::HostFailure)?;
        resolve_command_in_path(command, &path, &cwd).ok_or(RuntimeIoError::HostFailure)
    }

    /// Look up a live stream by its globally unique raw ID.
    pub fn stream(&self, raw: u64) -> Result<ByteStream, RuntimeIoError> {
        self.ensure_alive()?;
        self.inner
            .streams
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .get(&raw)
            .cloned()
            .map(|inner| ByteStream { inner })
            .ok_or(RuntimeIoError::StaleHandle)
    }

    /// Look up a live child by its globally unique raw ID.
    pub fn child(&self, raw: u64) -> Result<ChildHandle, RuntimeIoError> {
        self.ensure_alive()?;
        self.inner
            .children
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .get(&raw)
            .cloned()
            .map(|inner| ChildHandle { inner })
            .ok_or(RuntimeIoError::StaleHandle)
    }

    /// Look up a live timer by its globally unique raw ID.
    pub fn timer(&self, raw: u64) -> Result<TimerHandle, RuntimeIoError> {
        self.ensure_alive()?;
        self.inner
            .timers
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .get(&raw)
            .cloned()
            .map(|inner| TimerHandle { inner })
            .ok_or(RuntimeIoError::StaleHandle)
    }

    /// Close a stream, retire its notification, and release its resource slot.
    pub async fn drop_stream(&self, raw: u64) -> Result<(), RuntimeIoError> {
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let stream = self
            .inner
            .streams
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .remove(&raw)
            .ok_or(RuntimeIoError::StaleHandle)?;
        let _cleanup = CleanupGuard::new(&self.inner);
        drop(admission);
        self.wait_cleanup_gate().await;
        stream.stop();
        let linked_child = stream
            .linked_child
            .lock()
            .ok()
            .and_then(|child| child.as_ref().and_then(Weak::upgrade));
        if let Some(child) = linked_child {
            child.alive.store(false, Ordering::Release);
            child.shutdown.notify_one();
            loop {
                let notified = child.exited.notified();
                if child
                    .status
                    .lock()
                    .is_ok_and(|status| *status != ChildStatus::Running)
                {
                    break;
                }
                notified.await;
            }
        }
        self.join_resource_tasks(raw, true).await?;
        self.inner
            .coordination
            .retire_notification(stream.notification)
            .map_err(map_notification_error)?;
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }

    /// Kill a child, retire its notification, and release its resource slot.
    pub async fn drop_child(&self, raw: u64) -> Result<(), RuntimeIoError> {
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let child = self
            .inner
            .children
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .remove(&raw)
            .ok_or(RuntimeIoError::StaleHandle)?;
        let _cleanup = CleanupGuard::new(&self.inner);
        drop(admission);
        self.wait_cleanup_gate().await;
        child.alive.store(false, Ordering::Release);
        child.shutdown.notify_one();
        loop {
            let notified = child.exited.notified();
            if child
                .status
                .lock()
                .is_ok_and(|status| *status != ChildStatus::Running)
            {
                break;
            }
            notified.await;
        }
        self.join_resource_tasks(raw, false).await?;
        self.inner
            .coordination
            .retire_notification(child.notification)
            .map_err(map_notification_error)?;
        self.inner.children_count.fetch_sub(1, Ordering::AcqRel);
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }

    /// Cancel a timer, retire its notification, and release its resource slot.
    pub async fn drop_timer(&self, raw: u64) -> Result<(), RuntimeIoError> {
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        self.ensure_alive()?;
        let timer = self
            .inner
            .timers
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .remove(&raw)
            .ok_or(RuntimeIoError::StaleHandle)?;
        let _cleanup = CleanupGuard::new(&self.inner);
        drop(admission);
        self.wait_cleanup_gate().await;
        timer.alive.store(false, Ordering::Release);
        self.join_resource_tasks(raw, true).await?;
        self.inner
            .coordination
            .retire_notification(timer.notification)
            .map_err(map_notification_error)?;
        self.inner.timers_count.fetch_sub(1, Ordering::AcqRel);
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    }

    /// Kill children, close resources, abort and join background tasks, then
    /// retire every host-owned notification.
    pub async fn teardown(&self) {
        let Ok(admission) = self.inner.admission.lock() else {
            return;
        };
        if !self.inner.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        self.inner.connect_shutdown.send_replace(true);
        drop(admission);
        while self.inner.inflight_connects.load(Ordering::Acquire) != 0 {
            let changed = self.inner.inflight_changed.notified();
            if self.inner.inflight_connects.load(Ordering::Acquire) == 0 {
                break;
            }
            changed.await;
        }
        while self.inner.inflight_cleanups.load(Ordering::Acquire) != 0 {
            let changed = self.inner.cleanup_changed.notified();
            if self.inner.inflight_cleanups.load(Ordering::Acquire) == 0 {
                break;
            }
            changed.await;
        }
        let streams = self.inner.streams.lock().map_or_else(
            |_poisoned| Vec::new(),
            |streams| streams.values().cloned().collect(),
        );
        for stream in &streams {
            stream.stop();
        }
        let children = self.inner.children.lock().map_or_else(
            |_poisoned| Vec::new(),
            |children| children.values().cloned().collect(),
        );
        for child in &children {
            child.alive.store(false, Ordering::Release);
            child.shutdown.notify_one();
        }
        for child in &children {
            loop {
                let notified = child.exited.notified();
                let exited = child
                    .status
                    .lock()
                    .is_ok_and(|status| *status != ChildStatus::Running);
                if exited {
                    break;
                }
                notified.await;
            }
        }
        let timers = self.inner.timers.lock().map_or_else(
            |_poisoned| Vec::new(),
            |timers| timers.values().cloned().collect(),
        );
        for timer in &timers {
            timer.alive.store(false, Ordering::Release);
        }
        let tasks = self.inner.tasks.lock().map_or_else(
            |_poisoned| Vec::new(),
            |mut tasks| tasks.drain().flat_map(|(_id, tasks)| tasks).collect(),
        );
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _result = task.await;
        }
        for stream in streams {
            let _result = self
                .inner
                .coordination
                .retire_notification(stream.notification);
        }
        for child in children {
            let _result = self
                .inner
                .coordination
                .retire_notification(child.notification);
        }
        for timer in timers {
            let _result = self
                .inner
                .coordination
                .retire_notification(timer.notification);
        }
    }

    fn create_stream_admitted(&self) -> Result<ByteStream, RuntimeIoError> {
        self.reserve_resources(1)?;
        match self.create_stream_reserved() {
            Ok(stream) => Ok(stream),
            Err(error) => {
                self.inner.resources.fetch_sub(1, Ordering::AcqRel);
                Err(error)
            }
        }
    }

    fn create_stream_reserved(&self) -> Result<ByteStream, RuntimeIoError> {
        let notification = self
            .inner
            .coordination
            .create_notification()
            .map_err(map_notification_error)?;
        let inner = Arc::new(StreamInner {
            id: allocate_resource_id()?,
            generation: self.inner.generation,
            notification,
            coordination: self.inner.coordination.clone(),
            capacity: self.inner.config.stream_capacity,
            max_write_bytes: self.inner.config.max_write_bytes,
            alive: AtomicBool::new(true),
            signal_generation: AtomicU64::new(0),
            state: Mutex::new(StreamState::default()),
            changed: Condvar::new(),
            async_changed: Notify::new(),
            linked_child: Mutex::new(None),
        });
        self.inner
            .streams
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .insert(inner.id, Arc::clone(&inner));
        Ok(ByteStream { inner })
    }

    fn monitor_std_child(
        &self,
        child: Child,
        state: Arc<ChildInner>,
    ) -> Result<ChildHandle, RuntimeIoError> {
        let monitor = Arc::clone(&state);
        self.push_task(
            state.id,
            tokio::spawn(async move {
                monitor_child(child, &monitor).await;
            }),
        )?;
        Ok(ChildHandle { inner: state })
    }

    fn monitor_pty_child(
        &self,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
        master: Box<dyn MasterPty + Send>,
        state: Arc<ChildInner>,
    ) -> Result<ChildHandle, RuntimeIoError> {
        let monitor = Arc::clone(&state);
        self.push_task(
            state.id,
            tokio::task::spawn_blocking(move || {
                while monitor.alive.load(Ordering::Acquire) {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            monitor.complete(status.exit_code().try_into().ok());
                            return;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                        Err(_error) => break,
                    }
                }
                let _result = child.kill();
                let status = child
                    .wait()
                    .ok()
                    .and_then(|status| status.exit_code().try_into().ok());
                drop(master);
                monitor.complete(status);
            }),
        )?;
        Ok(ChildHandle { inner: state })
    }

    fn create_child(&self, resolved: PathBuf) -> Result<Arc<ChildInner>, RuntimeIoError> {
        let id = allocate_resource_id()?;
        let notification = self
            .inner
            .coordination
            .create_notification()
            .map_err(map_notification_error)?;
        let state = Arc::new(ChildInner {
            id,
            generation: self.inner.generation,
            notification,
            coordination: self.inner.coordination.clone(),
            resolved_command: resolved,
            signal_generation: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            shutdown: Notify::new(),
            exited: Notify::new(),
            status: Mutex::new(ChildStatus::Running),
        });
        self.inner
            .children
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .insert(state.id, Arc::clone(&state));
        Ok(state)
    }

    fn rollback_stream(&self, stream: &ByteStream) {
        stream.inner.stop();
        if let Ok(mut streams) = self.inner.streams.lock() {
            let _removed = streams.remove(&stream.raw());
        }
        let _result = self
            .inner
            .coordination
            .retire_notification(stream.notification());
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
    }

    fn rollback_child(&self, child: &Arc<ChildInner>) {
        child.alive.store(false, Ordering::Release);
        if let Ok(mut children) = self.inner.children.lock() {
            let _removed = children.remove(&child.id);
        }
        let _result = self
            .inner
            .coordination
            .retire_notification(child.notification);
        self.release_child();
    }

    fn spawn_reader<R>(&self, stream: Arc<StreamInner>, mut reader: R)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let resource = stream.id;
        let task = tokio::spawn(async move {
            let mut bytes = [0_u8; 4096];
            while stream.alive.load(Ordering::Acquire) {
                match reader.read(&mut bytes).await {
                    Ok(0) => break,
                    Ok(count) => {
                        let Some(chunk) = bytes.get(..count) else {
                            break;
                        };
                        stream.push_incoming_async(chunk).await;
                    }
                    Err(_error) => break,
                }
            }
            stream.finish_read();
        });
        let _result = self.push_task(resource, task);
    }

    fn spawn_writer<W>(&self, stream: Arc<StreamInner>, mut writer: W)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let resource = stream.id;
        let task = tokio::spawn(async move {
            while let Some(bytes) = stream.take_outgoing_async().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            stream.finish_write();
        });
        let _result = self.push_task(resource, task);
    }

    fn spawn_blocking_reader<R>(&self, stream: Arc<StreamInner>, mut reader: R)
    where
        R: Read + Send + 'static,
    {
        let resource = stream.id;
        let task = tokio::task::spawn_blocking(move || {
            let mut bytes = [0_u8; 4096];
            while stream.alive.load(Ordering::Acquire) {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        let Some(chunk) = bytes.get(..count) else {
                            break;
                        };
                        stream.push_incoming(chunk);
                    }
                    Err(_error) => break,
                }
            }
            stream.finish_read();
        });
        let _result = self.push_task(resource, task);
    }

    fn spawn_blocking_writer<W>(&self, stream: Arc<StreamInner>, mut writer: W)
    where
        W: Write + Send + 'static,
    {
        let resource = stream.id;
        let task = tokio::task::spawn_blocking(move || {
            while let Some(bytes) = stream.take_outgoing() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
            stream.finish_write();
        });
        let _result = self.push_task(resource, task);
    }

    fn push_task(&self, resource: u64, task: JoinHandle<()>) -> Result<(), RuntimeIoError> {
        self.inner
            .tasks
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .entry(resource)
            .or_default()
            .push(task);
        Ok(())
    }

    async fn join_resource_tasks(&self, resource: u64, abort: bool) -> Result<(), RuntimeIoError> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?
            .remove(&resource)
            .unwrap_or_default();
        if abort {
            for task in &tasks {
                task.abort();
            }
        }
        for task in tasks {
            let _result = task.await;
        }
        Ok(())
    }

    async fn wait_cleanup_gate(&self) {
        std::future::ready(()).await;
        #[cfg(test)]
        {
            let gate = self
                .inner
                .cleanup_gate
                .lock()
                .ok()
                .and_then(|mut gate| gate.take());
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.notified().await;
            }
        }
    }

    fn reserve_resources(&self, amount: usize) -> Result<(), RuntimeIoError> {
        reserve_counter(
            &self.inner.resources,
            amount,
            self.inner.config.max_resources,
        )
    }

    fn reserve_child(&self) -> Result<(), RuntimeIoError> {
        reserve_counter(
            &self.inner.children_count,
            1,
            self.inner.config.max_children,
        )?;
        if let Err(error) = self.reserve_resources(1) {
            self.inner.children_count.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    fn release_child(&self) {
        self.inner.children_count.fetch_sub(1, Ordering::AcqRel);
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
    }

    fn reserve_timer(&self) -> Result<(), RuntimeIoError> {
        reserve_counter(&self.inner.timers_count, 1, self.inner.config.max_timers)?;
        if let Err(error) = self.reserve_resources(1) {
            self.inner.timers_count.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }

    fn release_timer(&self) {
        self.inner.timers_count.fetch_sub(1, Ordering::AcqRel);
        self.inner.resources.fetch_sub(1, Ordering::AcqRel);
    }

    fn ensure_alive(&self) -> Result<(), RuntimeIoError> {
        if self.inner.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(RuntimeIoError::StaleHandle)
        }
    }
}

impl ByteStream {
    /// Globally unique guest-visible resource ID.
    pub fn raw(&self) -> u64 {
        self.inner.id
    }

    /// Owning actor generation.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Host readiness notification for both stream directions.
    pub fn notification(&self) -> NotificationId {
        self.inner.notification
    }

    /// Drain at most `max_bytes` without waiting.
    pub fn try_read(&self, max_bytes: usize) -> Result<StreamRead, RuntimeIoError> {
        self.inner.ensure_alive()?;
        if max_bytes == 0 {
            return Err(RuntimeIoError::InvalidArgument);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        if state.incoming.is_empty() {
            return Ok(if state.read_eof {
                StreamRead::Eof
            } else {
                StreamRead::WouldBlock
            });
        }
        let count = max_bytes.min(state.incoming.len());
        let bytes = state.incoming.drain(..count).collect();
        drop(state);
        self.inner.changed.notify_all();
        self.inner.async_changed.notify_waiters();
        self.inner.signal();
        Ok(StreamRead::Data(bytes))
    }

    /// Queue bytes without waiting, returning the admitted prefix length.
    pub fn try_write(&self, bytes: &[u8]) -> Result<usize, RuntimeIoError> {
        self.inner.ensure_alive()?;
        if bytes.is_empty() || bytes.len() > self.inner.max_write_bytes {
            return Err(RuntimeIoError::InvalidArgument);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        if state.write_closed {
            return Err(RuntimeIoError::Closed);
        }
        let available = self.inner.capacity.saturating_sub(state.outgoing.len());
        if available == 0 {
            return Err(RuntimeIoError::WouldBlock);
        }
        let count = available.min(bytes.len());
        state.outgoing.extend(bytes.iter().copied().take(count));
        drop(state);
        self.inner.changed.notify_all();
        self.inner.async_changed.notify_waiters();
        self.inner.signal();
        Ok(count)
    }

    /// Close the local write direction after queued bytes are flushed.
    pub fn close_write(&self) -> Result<(), RuntimeIoError> {
        self.inner.ensure_alive()?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)?;
        state.write_closed = true;
        drop(state);
        self.inner.changed.notify_all();
        self.inner.async_changed.notify_waiters();
        self.inner.signal();
        Ok(())
    }
}

impl StreamInner {
    fn ensure_alive(&self) -> Result<(), RuntimeIoError> {
        if self.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(RuntimeIoError::StaleHandle)
        }
    }

    fn signal(&self) {
        let generation = self.signal_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _result = self.coordination.signal(self.notification, generation);
    }

    fn push_incoming(&self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.alive.load(Ordering::Acquire) {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            while state.incoming.len() == self.capacity && self.alive.load(Ordering::Acquire) {
                let Ok(next) = self.changed.wait(state) else {
                    return;
                };
                state = next;
            }
            let count = (self.capacity - state.incoming.len()).min(bytes.len() - offset);
            let Some(chunk) = bytes.get(offset..offset + count) else {
                return;
            };
            state.incoming.extend(chunk.iter().copied());
            offset += count;
            drop(state);
            self.signal();
        }
    }

    async fn push_incoming_async(&self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.alive.load(Ordering::Acquire) {
            let notified = self.async_changed.notified();
            let count = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                let available = self.capacity.saturating_sub(state.incoming.len());
                let count = available.min(bytes.len() - offset);
                let Some(chunk) = bytes.get(offset..offset + count) else {
                    return;
                };
                state.incoming.extend(chunk.iter().copied());
                count
            };
            if count == 0 {
                notified.await;
            } else {
                offset += count;
                self.signal();
            }
        }
    }

    fn take_outgoing(&self) -> Option<Vec<u8>> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        while state.outgoing.is_empty() && !state.write_closed && self.alive.load(Ordering::Acquire)
        {
            let Ok(next) = self.changed.wait(state) else {
                return None;
            };
            state = next;
        }
        if state.outgoing.is_empty() {
            return None;
        }
        let bytes = state.outgoing.drain(..).collect();
        drop(state);
        self.signal();
        Some(bytes)
    }

    async fn take_outgoing_async(&self) -> Option<Vec<u8>> {
        loop {
            let notified = self.async_changed.notified();
            {
                let Ok(mut state) = self.state.lock() else {
                    return None;
                };
                if !state.outgoing.is_empty() {
                    let bytes = state.outgoing.drain(..).collect();
                    drop(state);
                    self.signal();
                    return Some(bytes);
                }
                if state.write_closed || !self.alive.load(Ordering::Acquire) {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn finish_read(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.read_eof = true;
        }
        self.signal();
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }

    fn finish_write(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.write_closed = true;
        }
        self.signal();
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }

    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }
}

impl ChildHandle {
    /// Globally unique guest-visible resource ID.
    pub fn raw(&self) -> u64 {
        self.inner.id
    }

    /// Owning actor generation.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Host readiness notification for child exit.
    pub fn notification(&self) -> NotificationId {
        self.inner.notification
    }

    /// Absolute PATH-selected executable, preserving its command identity.
    pub fn resolved_command(&self) -> &Path {
        &self.inner.resolved_command
    }

    /// Query child state without waiting.
    pub fn status(&self) -> Result<ChildStatus, RuntimeIoError> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err(RuntimeIoError::StaleHandle);
        }
        self.inner
            .status
            .lock()
            .map(|status| *status)
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)
    }
}

fn resolve_command_in_path(command: &str, path: &std::ffi::OsStr, cwd: &Path) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|directory| directory.join(command))
        .map(|candidate| {
            if candidate.is_absolute() {
                candidate
            } else {
                cwd.join(candidate)
            }
        })
        .find(|candidate| candidate.is_file())
}

impl ChildInner {
    fn complete(&self, code: Option<i32>) {
        if let Ok(mut status) = self.status.lock() {
            *status = ChildStatus::Exited(code);
        }
        let generation = self.signal_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _result = self.coordination.signal(self.notification, generation);
        self.exited.notify_waiters();
    }
}

impl TimerHandle {
    /// Globally unique guest-visible resource ID.
    pub fn raw(&self) -> u64 {
        self.inner.id
    }

    /// Owning actor generation.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Host readiness notification for expiry.
    pub fn notification(&self) -> NotificationId {
        self.inner.notification
    }

    /// Query timer state without waiting.
    pub fn status(&self) -> Result<TimerStatus, RuntimeIoError> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err(RuntimeIoError::StaleHandle);
        }
        self.inner
            .status
            .lock()
            .map(|status| *status)
            .map_err(|_poisoned| RuntimeIoError::StaleHandle)
    }
}

async fn monitor_child(mut child: Child, state: &ChildInner) {
    tokio::select! {
        status = child.wait() => state.complete(status.ok().and_then(|status| status.code())),
        () = state.shutdown.notified() => {
            let _result = child.start_kill();
            state.complete(child.wait().await.ok().and_then(|status| status.code()));
        }
    }
}

fn reserve_counter(
    counter: &AtomicUsize,
    amount: usize,
    maximum: usize,
) -> Result<(), RuntimeIoError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(amount).filter(|next| *next <= maximum)
        })
        .map(|_previous| ())
        .map_err(|_current| RuntimeIoError::LimitReached)
}

fn allocate_resource_id() -> Result<u64, RuntimeIoError> {
    NEXT_RESOURCE_ID
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .map_err(|_exhausted| RuntimeIoError::LimitReached)
}

const fn map_notification_error(error: NotificationError) -> RuntimeIoError {
    match error {
        NotificationError::Stopped => RuntimeIoError::StaleHandle,
        NotificationError::LimitReached | NotificationError::BacklogFull => {
            RuntimeIoError::LimitReached
        }
        NotificationError::InvalidHandle
        | NotificationError::InvalidGeneration
        | NotificationError::AlreadyWatched => RuntimeIoError::HostFailure,
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, ensure};
    use tokio::time::{advance, timeout};

    use super::*;
    use crate::CoordinationConfig;

    #[cfg(unix)]
    #[test]
    fn path_resolution_preserves_symlink_command_identity() -> Result<()> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let target = root.path().join("multicall");
        std::fs::write(&target, b"target")?;
        let selected = root.path().join("tr");
        symlink(&target, &selected)?;
        let resolved = resolve_command_in_path("tr", root.path().as_os_str(), root.path())
            .context("resolve PATH-selected symlink")?;
        ensure!(resolved == selected);
        ensure!(resolved.canonicalize()? == target);
        Ok(())
    }

    fn test_hub(commands: &[&str]) -> Result<RuntimeIoHub> {
        let (coordination, _events) = CoordinationHub::new(
            19,
            CoordinationConfig {
                max_notifications: 32,
                max_pollables: 32,
                max_wait_set: 8,
            },
        )?;
        Ok(RuntimeIoHub::new(
            19,
            RuntimeIoConfig {
                capabilities: RuntimeIoCapabilities {
                    commands: commands.iter().map(ToString::to_string).collect(),
                    allow_pty: true,
                    allow_loopback: true,
                },
                max_resources: 16,
                max_children: 4,
                max_timers: 4,
                stream_capacity: 4,
                max_write_bytes: 8,
            },
            coordination,
        )?)
    }

    #[tokio::test]
    async fn bounded_stream_backpressure_and_eof_are_observable() -> Result<()> {
        let hub = test_hub(&["sh"])?;
        let bounded = hub.create_stream_admitted()?;
        ensure!(bounded.try_write(b"12345678")? == 4);
        ensure!(bounded.try_write(b"x") == Err(RuntimeIoError::WouldBlock));
        let process = hub.spawn_process("sh", &["-c".into(), "printf 12345678".into()])?;
        let mut output = Vec::new();
        loop {
            match process.stdout.try_read(8)? {
                StreamRead::Data(bytes) => output.extend(bytes),
                StreamRead::WouldBlock => tokio::task::yield_now().await,
                StreamRead::Eof => break,
            }
        }
        ensure!(output == b"12345678");
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_admission_rolls_back_and_drop_reuses_capacity() -> Result<()> {
        let (coordination, _events) = CoordinationHub::new(23, CoordinationConfig::default())?;
        let hub = RuntimeIoHub::new(
            23,
            RuntimeIoConfig {
                capabilities: RuntimeIoCapabilities {
                    commands: vec!["sh".into()],
                    allow_pty: false,
                    allow_loopback: false,
                },
                max_resources: 2,
                max_children: 1,
                max_timers: 2,
                stream_capacity: 8,
                max_write_bytes: 8,
            },
            coordination,
        )?;
        ensure!(matches!(
            hub.spawn_process("sh", &["-c".into(), "exit 0".into()]),
            Err(RuntimeIoError::LimitReached)
        ));
        ensure!(hub.inner.resources.load(Ordering::Acquire) == 0);
        ensure!(hub.inner.children_count.load(Ordering::Acquire) == 0);
        let timer = hub.create_timer(Duration::from_mins(1))?;
        let raw = timer.raw();
        ensure!(hub.timer(raw)?.raw() == raw);
        hub.drop_timer(raw).await?;
        ensure!(matches!(hub.timer(raw), Err(RuntimeIoError::StaleHandle)));
        let replacement = hub.create_timer(Duration::from_mins(1))?;
        ensure!(replacement.raw() != raw);
        ensure!(hub.inner.tasks.lock().is_ok_and(|tasks| tasks.len() == 1));
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test]
    async fn pty_is_a_terminal_and_blocked_resources_teardown() -> Result<()> {
        let hub = test_hub(&["sh"])?;
        let cwd = env::current_dir()?.canonicalize()?;
        let cwd_marker = format!("PTY-CWD={}", cwd.display()).into_bytes();
        let pty = hub.spawn_pty(
            "sh",
            &[
                "-c".into(),
                "test -t 0 && test -t 1 || exit 42; printf PTY-CWD=; pwd -P; printf PTY-OK; sleep 60".into(),
            ],
            24,
            80,
        )?;
        ensure!(pty.terminal.raw() != pty.child.raw());
        let mut output = Vec::new();
        timeout(Duration::from_secs(2), async {
            while !output
                .windows(b"PTY-OK".len())
                .any(|window| window == b"PTY-OK")
            {
                match pty.terminal.try_read(64)? {
                    StreamRead::Data(bytes) => output.extend(bytes),
                    StreamRead::WouldBlock => tokio::task::yield_now().await,
                    StreamRead::Eof => break,
                }
            }
            Ok::<(), RuntimeIoError>(())
        })
        .await
        .context("PTY child did not report terminal semantics")??;
        ensure!(
            output
                .windows(b"PTY-OK".len())
                .any(|window| window == b"PTY-OK")
        );
        ensure!(
            output
                .windows(cwd_marker.len())
                .any(|window| window == cwd_marker)
        );
        timeout(Duration::from_secs(2), hub.teardown())
            .await
            .context("PTY teardown did not join blocked host resources")?;
        ensure!(pty.child.status() == Err(RuntimeIoError::StaleHandle));
        ensure!(pty.terminal.try_read(1) == Err(RuntimeIoError::StaleHandle));
        Ok(())
    }

    #[tokio::test]
    async fn connect_cannot_publish_after_teardown_begins() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let hub = test_hub(&[])?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *hub.inner
            .connect_gate
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("gate poisoned"))? =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        let connecting_hub = hub.clone();
        let connecting =
            tokio::spawn(async move { connecting_hub.connect_loopback(address).await });
        let accepting = tokio::spawn(async move { listener.accept().await });
        entered.notified().await;
        hub.teardown().await;
        release.notify_one();
        ensure!(matches!(
            connecting.await?,
            Err(RuntimeIoError::StaleHandle)
        ));
        accepting.abort();
        ensure!(
            hub.inner
                .streams
                .lock()
                .is_ok_and(|streams| streams.is_empty())
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_connect_rolls_back_its_reserved_capacity() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let hub = test_hub(&[])?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *hub.inner
            .connect_gate
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("gate poisoned"))? =
            Some((Arc::clone(&entered), release));
        let connecting_hub = hub.clone();
        let connecting =
            tokio::spawn(async move { connecting_hub.connect_loopback(address).await });
        let accepting = tokio::spawn(async move { listener.accept().await });
        entered.notified().await;
        connecting.abort();
        let _result = connecting.await;
        ensure!(hub.inner.resources.load(Ordering::Acquire) == 0);
        ensure!(
            hub.inner
                .streams
                .lock()
                .is_ok_and(|streams| streams.is_empty())
        );
        accepting.abort();
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn teardown_cannot_miss_connect_registration() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let hub = test_hub(&[])?;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *hub.inner
            .connect_registration_gate
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("registration gate poisoned"))? =
            Some((entered_tx, release_rx));
        let connecting_hub = hub.clone();
        let connecting =
            tokio::spawn(async move { connecting_hub.connect_loopback(address).await });
        tokio::task::spawn_blocking(move || entered_rx.recv()).await??;
        let teardown_hub = hub.clone();
        let teardown = tokio::spawn(async move { teardown_hub.teardown().await });
        tokio::task::yield_now().await;
        ensure!(!teardown.is_finished());
        release_tx.send(())?;
        teardown.await?;
        ensure!(matches!(
            connecting.await?,
            Err(RuntimeIoError::StaleHandle)
        ));
        ensure!(hub.inner.inflight_connects.load(Ordering::Acquire) == 0);
        Ok(())
    }

    #[tokio::test]
    async fn teardown_waits_for_an_already_claimed_drop() -> Result<()> {
        let hub = test_hub(&[])?;
        let timer = hub.create_timer(Duration::from_mins(1))?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *hub.inner
            .cleanup_gate
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("cleanup gate poisoned"))? =
            Some((Arc::clone(&entered), Arc::clone(&release)));
        let dropping_hub = hub.clone();
        let dropping = tokio::spawn(async move { dropping_hub.drop_timer(timer.raw()).await });
        entered.notified().await;
        let teardown_hub = hub.clone();
        let teardown = tokio::spawn(async move { teardown_hub.teardown().await });
        tokio::task::yield_now().await;
        ensure!(!teardown.is_finished());
        release.notify_one();
        dropping.await??;
        teardown.await?;
        ensure!(hub.inner.inflight_cleanups.load(Ordering::Acquire) == 0);
        ensure!(hub.inner.tasks.lock().is_ok_and(|tasks| tasks.is_empty()));
        Ok(())
    }

    #[tokio::test]
    async fn explicit_drop_churn_reaps_tasks_before_reusing_quotas() -> Result<()> {
        let hub = test_hub(&["sh"])?;
        for _iteration in 0..16 {
            let process = hub.spawn_process("sh", &["-c".into(), "exit 0".into()])?;
            while process.child.status()? == ChildStatus::Running {
                tokio::task::yield_now().await;
            }
            hub.drop_child(process.child.raw()).await?;
            hub.drop_stream(process.stdin.raw()).await?;
            hub.drop_stream(process.stdout.raw()).await?;
            hub.drop_stream(process.stderr.raw()).await?;
            ensure!(hub.inner.resources.load(Ordering::Acquire) == 0);
            ensure!(hub.inner.children_count.load(Ordering::Acquire) == 0);
            ensure!(hub.inner.tasks.lock().is_ok_and(|tasks| tasks.is_empty()));
        }
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test]
    async fn blocked_loopback_reader_is_closed_and_joined_on_teardown() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (_socket, _address) = listener.accept().await?;
            std::future::pending::<()>().await;
            #[allow(
                unreachable_code,
                reason = "pending socket holder ends only by cancellation"
            )]
            Ok::<(), std::io::Error>(())
        });
        let hub = test_hub(&[])?;
        let stream = hub.connect_loopback(address).await?;
        timeout(Duration::from_secs(2), hub.teardown())
            .await
            .context("loopback teardown did not close a blocked reader")?;
        ensure!(stream.try_read(1) == Err(RuntimeIoError::StaleHandle));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn child_exit_and_resolved_command_are_queryable() -> Result<()> {
        let hub = test_hub(&["sh"])?;
        let process = hub.spawn_process("sh", &["-c".into(), "exit 7".into()])?;
        ensure!(process.child.resolved_command().is_absolute());
        timeout(Duration::from_secs(2), async {
            while process.child.status() == Ok(ChildStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("child did not exit")?;
        ensure!(process.child.status()? == ChildStatus::Exited(Some(7)));
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn timers_expire_in_deadline_order() -> Result<()> {
        let hub = test_hub(&[])?;
        let later = hub.create_timer(Duration::from_millis(20))?;
        let sooner = hub.create_timer(Duration::from_millis(10))?;
        advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        ensure!(sooner.status()? == TimerStatus::Expired);
        ensure!(later.status()? == TimerStatus::Pending);
        advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        ensure!(later.status()? == TimerStatus::Expired);
        hub.teardown().await;
        Ok(())
    }

    #[tokio::test]
    async fn teardown_invalidates_every_handle_and_retires_notifications() -> Result<()> {
        let hub = test_hub(&["sh"])?;
        let timer = hub.create_timer(Duration::from_mins(1))?;
        let process = hub.spawn_process("sh", &["-c".into(), "sleep 60".into()])?;
        let stale_notification = timer.notification();
        let coordination = hub.inner.coordination.clone();
        hub.teardown().await;
        ensure!(timer.status() == Err(RuntimeIoError::StaleHandle));
        ensure!(process.child.status() == Err(RuntimeIoError::StaleHandle));
        ensure!(process.stdout.try_read(1) == Err(RuntimeIoError::StaleHandle));
        ensure!(
            coordination.signal(stale_notification, 2) == Err(NotificationError::InvalidHandle)
        );
        Ok(())
    }
}
