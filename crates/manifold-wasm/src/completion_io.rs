//! Shared completion-based filesystem I/O for plugin actors.

use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{FileTypeExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use tokio::{
    sync::{Notify, mpsc},
    task::{AbortHandle, JoinHandle, JoinSet},
};

/// A stable operation identity within one actor generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId {
    generation: u64,
    sequence: u64,
}

impl OperationId {
    /// Construct an actor-scoped operation ID.
    pub const fn new(generation: u64, sequence: u64) -> Self {
        Self {
            generation,
            sequence,
        }
    }

    /// Actor generation which owns this operation.
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Guest-visible actor-scoped operation number.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Terminal result delivered on an actor's reserved completion lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorCompletion {
    /// Operation which reached a terminal state.
    pub operation: OperationId,
    /// Typed terminal result.
    pub kind: ActorCompletionKind,
}

/// Typed completion payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorCompletionKind {
    /// A read completed with these bytes.
    Read { bytes: Vec<u8> },
    /// A write completed with this byte count.
    Write { bytes_written: u64 },
    /// Cancellation won before a result was delivered.
    Cancelled,
    /// The operation failed.
    Failed { message: String },
}

/// One filesystem capability shared as authority by an actor and its workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemGrant {
    /// Host directory opened as the capability root.
    pub host_path: PathBuf,
    /// Guest-visible preopen path.
    pub guest_path: String,
    /// Whether writes are permitted.
    pub writable: bool,
}

/// Bounds and authority for one completion-I/O supervisor.
#[derive(Debug, Clone)]
pub struct CompletionIoConfig {
    /// Optional filesystem authority.
    pub filesystem: Option<FilesystemGrant>,
    /// Maximum operations which may be outstanding.
    pub max_outstanding: usize,
    /// Maximum read or write payload.
    pub max_completion_bytes: usize,
}

/// Submission failure which is known synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompletionIoError {
    /// Owning actor has stopped.
    #[error("actor has stopped")]
    ActorStopped,
    /// Outstanding-operation bound reached.
    #[error("actor I/O limit reached")]
    LimitReached,
    /// Supervisor command queue is full.
    #[error("actor I/O command backlog is full")]
    BacklogFull,
    /// Filesystem capability does not permit this operation.
    #[error("actor has no matching filesystem capability")]
    NotPermitted,
    /// Path, offset, or payload is invalid.
    #[error("invalid I/O argument")]
    InvalidArgument,
}

/// Nonblocking completion-I/O submission handle.
#[derive(Clone, Debug)]
pub struct CompletionIo {
    inner: Arc<CompletionIoInner>,
}

/// Observable state for one actor-generation completion supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionIoStats {
    /// Actor generation which owns every admitted operation.
    pub generation: u64,
    /// Operations which have not yet reached a terminal result.
    pub outstanding: usize,
    /// Configured admission bound.
    pub max_outstanding: usize,
    /// Whether new operations may still be admitted.
    pub alive: bool,
    /// Whether a filesystem capability was granted.
    pub permitted: bool,
    /// Whether that capability permits writes.
    pub writable: bool,
}

#[derive(Debug)]
struct CompletionIoInner {
    generation: u64,
    next_sequence: AtomicU64,
    outstanding: AtomicUsize,
    max_outstanding: usize,
    max_completion_bytes: usize,
    writable: bool,
    permitted: bool,
    alive: AtomicBool,
    shutdown: Notify,
    commands: mpsc::Sender<IoSupervisorCommand>,
    #[cfg(test)]
    supervisor_exited: AtomicBool,
    #[cfg(test)]
    supervisor_exit: Notify,
}

#[derive(Debug)]
enum IoSupervisorCommand {
    Start(IoRequest),
    Cancel(OperationId),
}

#[derive(Debug)]
struct IoRequest {
    id: OperationId,
    operation: IoOperation,
}

#[derive(Debug)]
enum IoOperation {
    Read {
        path: PathBuf,
        offset: u64,
        max_bytes: usize,
    },
    Write {
        path: PathBuf,
        offset: u64,
        bytes: Vec<u8>,
    },
}

impl CompletionIo {
    /// Submit a read and return immediately with its operation ID.
    pub fn submit_read(
        &self,
        path: impl AsRef<Path>,
        offset: u64,
        max_bytes: usize,
    ) -> std::result::Result<OperationId, CompletionIoError> {
        if max_bytes > self.inner.max_completion_bytes {
            return Err(CompletionIoError::InvalidArgument);
        }
        self.submit(IoOperation::Read {
            path: validate_capability_path(path.as_ref())?,
            offset,
            max_bytes,
        })
    }

    /// Submit a write and return immediately with its operation ID.
    pub fn submit_write(
        &self,
        path: impl AsRef<Path>,
        offset: u64,
        bytes: Vec<u8>,
    ) -> std::result::Result<OperationId, CompletionIoError> {
        if !self.inner.writable {
            return Err(CompletionIoError::NotPermitted);
        }
        if bytes.len() > self.inner.max_completion_bytes {
            return Err(CompletionIoError::InvalidArgument);
        }
        self.submit(IoOperation::Write {
            path: validate_capability_path(path.as_ref())?,
            offset,
            bytes,
        })
    }

    /// Queue a cancellation request.
    ///
    /// `true` means the request entered the supervisor queue, not that it won
    /// a race with operation completion. If cancellation wins before result
    /// delivery, the supervisor emits exactly one terminal `Cancelled`
    /// completion.
    pub fn cancel(&self, operation: OperationId) -> bool {
        if operation.generation != self.inner.generation
            || !self.inner.alive.load(Ordering::Acquire)
        {
            return false;
        }
        self.inner
            .commands
            .try_send(IoSupervisorCommand::Cancel(operation))
            .is_ok()
    }

    /// Actor generation owned by this handle.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Snapshot admission and lifecycle state without blocking the actor.
    pub fn stats(&self) -> CompletionIoStats {
        CompletionIoStats {
            generation: self.inner.generation,
            outstanding: self.inner.outstanding.load(Ordering::Acquire),
            max_outstanding: self.inner.max_outstanding,
            alive: self.inner.alive.load(Ordering::Acquire),
            permitted: self.inner.permitted,
            writable: self.inner.writable,
        }
    }

    fn submit(
        &self,
        operation: IoOperation,
    ) -> std::result::Result<OperationId, CompletionIoError> {
        if !self.inner.permitted {
            return Err(CompletionIoError::NotPermitted);
        }
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err(CompletionIoError::ActorStopped);
        }
        self.inner
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.inner.max_outstanding).then_some(current + 1)
            })
            .map_err(|_limit| CompletionIoError::LimitReached)?;
        let sequence = self
            .inner
            .next_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| {
                self.inner.outstanding.fetch_sub(1, Ordering::AcqRel);
                CompletionIoError::LimitReached
            })?;
        let id = OperationId::new(self.inner.generation, sequence);
        if self
            .inner
            .commands
            .try_send(IoSupervisorCommand::Start(IoRequest { id, operation }))
            .is_err()
        {
            self.inner.outstanding.fetch_sub(1, Ordering::AcqRel);
            return Err(CompletionIoError::BacklogFull);
        }
        Ok(id)
    }

    /// Stop accepting operations and wake the owned supervisor.
    pub(crate) fn stop(&self) {
        self.inner.alive.store(false, Ordering::Release);
        self.inner.shutdown.notify_one();
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_supervisor_exit(&self) {
        loop {
            let notified = self.inner.supervisor_exit.notified();
            if self.inner.supervisor_exited.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// Start one owned completion supervisor.
pub(crate) fn start_completion_io(
    generation: u64,
    config: &CompletionIoConfig,
) -> Result<(
    CompletionIo,
    mpsc::Receiver<ActorCompletion>,
    JoinHandle<()>,
)> {
    ensure!(config.max_outstanding > 0, "I/O limit must be non-zero");
    ensure!(
        config.max_completion_bytes > 0,
        "completion byte limit must be non-zero"
    );
    let root = config
        .filesystem
        .as_ref()
        .map(|grant| {
            Dir::open_ambient_dir(&grant.host_path, ambient_authority()).with_context(|| {
                format!(
                    "open completion I/O capability root {}",
                    grant.host_path.display()
                )
            })
        })
        .transpose()?
        .map(Arc::new);
    let writable = config
        .filesystem
        .as_ref()
        .is_some_and(|grant| grant.writable);
    let command_capacity = config
        .max_outstanding
        .checked_mul(2)
        .context("I/O command capacity overflow")?;
    let (command_tx, command_rx) = mpsc::channel(command_capacity);
    let (completion_tx, completion_rx) = mpsc::channel(config.max_outstanding);
    let inner = Arc::new(CompletionIoInner {
        generation,
        next_sequence: AtomicU64::new(1),
        outstanding: AtomicUsize::new(0),
        max_outstanding: config.max_outstanding,
        max_completion_bytes: config.max_completion_bytes,
        writable,
        permitted: root.is_some(),
        alive: AtomicBool::new(true),
        shutdown: Notify::new(),
        commands: command_tx,
        #[cfg(test)]
        supervisor_exited: AtomicBool::new(false),
        #[cfg(test)]
        supervisor_exit: Notify::new(),
    });
    let io = CompletionIo {
        inner: Arc::clone(&inner),
    };
    let task = tokio::spawn(run_completion_io_supervisor(
        root,
        writable,
        inner,
        command_rx,
        completion_tx,
    ));
    Ok((io, completion_rx, task))
}

async fn run_completion_io_supervisor(
    root: Option<Arc<Dir>>,
    writable: bool,
    state: Arc<CompletionIoInner>,
    mut commands: mpsc::Receiver<IoSupervisorCommand>,
    completions: mpsc::Sender<ActorCompletion>,
) {
    let mut tasks = JoinSet::new();
    let mut running: HashMap<OperationId, AbortHandle> = HashMap::new();
    loop {
        tokio::select! {
            () = state.shutdown.notified() => break,
            command = commands.recv() => match command {
                Some(IoSupervisorCommand::Start(request)) => {
                    let id = request.id;
                    let root = root.as_ref().map(Arc::clone);
                    let abort = tasks.spawn(async move {
                        let result = execute_io_async(root, writable, request.operation).await;
                        (id, result)
                    });
                    running.insert(id, abort);
                }
                Some(IoSupervisorCommand::Cancel(id)) => {
                    if let Some(abort) = running.remove(&id) {
                        abort.abort();
                        finish_completion(
                            &state,
                            &completions,
                            ActorCompletion { operation: id, kind: ActorCompletionKind::Cancelled },
                        ).await;
                    }
                }
                None => break,
            },
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok((id, result))) = result
                    && running.remove(&id).is_some()
                {
                    let kind = result.unwrap_or_else(|error| ActorCompletionKind::Failed {
                        message: error.to_string(),
                    });
                    finish_completion(
                        &state,
                        &completions,
                        ActorCompletion { operation: id, kind },
                    ).await;
                }
            }
        }
    }
    state.alive.store(false, Ordering::Release);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let abandoned = running.len();
    if abandoned > 0 {
        state.outstanding.fetch_sub(abandoned, Ordering::AcqRel);
    }
    #[cfg(test)]
    {
        state.supervisor_exited.store(true, Ordering::Release);
        state.supervisor_exit.notify_waiters();
    }
}

async fn execute_io_async(
    root: Option<Arc<Dir>>,
    writable: bool,
    operation: IoOperation,
) -> Result<ActorCompletionKind> {
    let root = root.context("filesystem capability is not granted")?;
    match operation {
        IoOperation::Read {
            path,
            offset,
            max_bytes,
        } => {
            let bytes = completion_read_file(
                CompletionFilesystem { root, writable },
                path,
                offset,
                max_bytes,
            )
            .await?;
            Ok(ActorCompletionKind::Read { bytes })
        }
        operation @ IoOperation::Write { .. } => tokio::task::spawn_blocking(move || {
            execute_io(Some(root.as_ref()), writable, operation)
        })
        .await
        .context("join completion I/O write")?,
    }
}

async fn finish_completion(
    state: &CompletionIoInner,
    completions: &mpsc::Sender<ActorCompletion>,
    completion: ActorCompletion,
) {
    if state.alive.load(Ordering::Acquire) && completion.operation.generation == state.generation {
        let _result = completions.send(completion).await;
    }
    state.outstanding.fetch_sub(1, Ordering::AcqRel);
}

fn execute_io(
    root: Option<&Dir>,
    writable: bool,
    operation: IoOperation,
) -> Result<ActorCompletionKind> {
    let root = root.context("filesystem capability is not granted")?;
    match operation {
        IoOperation::Read {
            path,
            offset,
            max_bytes,
        } => {
            let mut options = OpenOptions::new();
            options.read(true).nonblock(true);
            let mut file = root
                .open_with(&path, &options)
                .with_context(|| format!("open {} for completion read", path.display()))?;
            ensure!(
                file.metadata()?.file_type().is_file(),
                "completion reads require a regular file"
            );
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; max_bytes];
            let count = file.read(&mut bytes)?;
            bytes.truncate(count);
            Ok(ActorCompletionKind::Read { bytes })
        }
        IoOperation::Write {
            path,
            offset,
            bytes,
        } => {
            ensure!(writable, "filesystem capability is read-only");
            let mut options = OpenOptions::new();
            options.write(true).create(true).nonblock(true);
            let mut file = root
                .open_with(&path, &options)
                .with_context(|| format!("open {} for completion write", path.display()))?;
            ensure!(
                file.metadata()?.file_type().is_file(),
                "completion writes require a regular file"
            );
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&bytes)?;
            Ok(ActorCompletionKind::Write {
                bytes_written: u64::try_from(bytes.len())?,
            })
        }
    }
}

#[derive(Clone)]
struct CompletionFilesystem {
    root: Arc<Dir>,
    writable: bool,
}

async fn completion_read_file(
    filesystem: CompletionFilesystem,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    // Opening a descriptor and handling a regular file are short blocking
    // setup operations. A FIFO/socket is returned in nonblocking mode and its
    // readiness wait stays cancellable on Tokio's AsyncFd path.
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_completion_read(filesystem, path, offset, max_bytes)
    })
    .await
    .context("join completion filesystem preparation")??;
    let (file, max_bytes) = match prepared {
        PreparedCompletionRead::Complete(bytes) => return Ok(bytes),
        PreparedCompletionRead::Async(file, max_bytes) => (file, max_bytes),
    };

    let async_file = tokio::io::unix::AsyncFd::new(file)?;
    let mut bytes = vec![0; max_bytes];
    loop {
        let mut guard = async_file.readable().await?;
        match guard.try_io(|inner| inner.get_ref().read(&mut bytes)) {
            Ok(result) => {
                let count = result?;
                bytes.truncate(count);
                return Ok(bytes);
            }
            Err(_would_block) => {}
        }
    }
}

enum PreparedCompletionRead {
    Complete(Vec<u8>),
    Async(std::fs::File, usize),
}

fn prepare_completion_read(
    filesystem: CompletionFilesystem,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
) -> Result<PreparedCompletionRead> {
    let mut options = OpenOptions::new();
    options.read(true).nonblock(true);
    // Opening both ends avoids an immediate EOF before an external FIFO
    // producer connects. The extra write access requires a writable grant.
    if filesystem.writable {
        options.write(true);
    }
    let mut file = filesystem
        .root
        .open_with(&path, &options)
        .with_context(|| format!("open {} for completion read", path.display()))?;
    let file_type = file.metadata()?.file_type();
    if !file_type.is_fifo() && !file_type.is_socket() {
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; max_bytes];
        let count = file.read(&mut bytes)?;
        bytes.truncate(count);
        return Ok(PreparedCompletionRead::Complete(bytes));
    }
    Ok(PreparedCompletionRead::Async(file.into_std(), max_bytes))
}

fn validate_capability_path(path: &Path) -> std::result::Result<PathBuf, CompletionIoError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(CompletionIoError::InvalidArgument);
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(CompletionIoError::InvalidArgument);
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, ensure};
    use tokio::time::{Duration, timeout};

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_bypasses_a_full_operation_command_queue() -> Result<()> {
        let (command_tx, command_rx) = mpsc::channel(1);
        let fake = OperationId::new(73, 1);
        command_tx
            .try_send(IoSupervisorCommand::Cancel(fake))
            .map_err(|error| anyhow::anyhow!("fill I/O command queue: {error}"))?;
        let state = Arc::new(CompletionIoInner {
            generation: 73,
            next_sequence: AtomicU64::new(2),
            outstanding: AtomicUsize::new(0),
            max_outstanding: 1,
            max_completion_bytes: 32,
            writable: false,
            permitted: false,
            alive: AtomicBool::new(true),
            shutdown: Notify::new(),
            commands: command_tx,
            supervisor_exited: AtomicBool::new(false),
            supervisor_exit: Notify::new(),
        });
        let io = CompletionIo {
            inner: Arc::clone(&state),
        };
        let (completion_tx, _completion_rx) = mpsc::channel(1);
        io.stop();
        let task = tokio::spawn(run_completion_io_supervisor(
            None,
            false,
            state,
            command_rx,
            completion_tx,
        ));
        timeout(Duration::from_millis(100), task)
            .await
            .context("full I/O queue blocked shutdown")??;
        let stats = io.stats();
        ensure!(!stats.alive && stats.outstanding == 0);
        ensure!(stats.generation == 73 && stats.max_outstanding == 1);
        Ok(())
    }

    #[test]
    fn exhausted_operation_ids_never_wrap_or_reuse() -> Result<()> {
        let (commands, _receiver) = mpsc::channel(2);
        let io = CompletionIo {
            inner: Arc::new(CompletionIoInner {
                generation: 73,
                next_sequence: AtomicU64::new(u64::MAX),
                outstanding: AtomicUsize::new(0),
                max_outstanding: 2,
                max_completion_bytes: 32,
                writable: false,
                permitted: true,
                alive: AtomicBool::new(true),
                shutdown: Notify::new(),
                commands,
                supervisor_exited: AtomicBool::new(false),
                supervisor_exit: Notify::new(),
            }),
        };
        ensure!(
            io.submit_read("file", 0, 1) == Err(CompletionIoError::LimitReached),
            "the exhausted allocator admitted an operation"
        );
        ensure!(
            io.submit_read("file", 0, 1) == Err(CompletionIoError::LimitReached),
            "the exhausted allocator wrapped and reused an operation ID"
        );
        ensure!(io.inner.outstanding.load(Ordering::Acquire) == 0);
        let stats = io.stats();
        ensure!(stats.permitted && !stats.writable && stats.outstanding == 0);
        Ok(())
    }
}
