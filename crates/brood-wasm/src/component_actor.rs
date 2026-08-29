//! Component Model actor runtime.
//!
//! Unlike the legacy core-Wasm epoch runner, this runtime keeps one component
//! instance and one Store alive for an actor's complete lifetime. Actor
//! dispatch calls and actor-private worker calls enter that same instance via
//! Wasmtime's concurrent Component Model API, so guest memory and globals are
//! shared without serialising the Store behind a host lock.

#![allow(
    clippy::let_underscore_must_use,
    clippy::map_err_ignore,
    clippy::needless_pass_by_ref_mut,
    clippy::option_if_let_else,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::uninlined_format_args,
    clippy::unnecessary_debug_formatting,
    reason = "the concurrent runtime intentionally ignores observer backpressure and keeps generated Component ABI shapes explicit"
)]

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "brood-plugin",
        imports: { default: trappable },
        exports: { default: async | store },
        require_store_data_send: true,
    });
}

use crate::actor_kernel::{ActorRuntimeKernel, allocate_generation};
use crate::completion_io::{
    ActorCompletion, ActorCompletionKind, CompletionIo, CompletionIoConfig, CompletionIoError,
    FilesystemGrant, OperationId, start_completion_io_on as start_shared_completion_io_on,
};
use crate::coordination::{
    ActorWatchResult, CoordinationConfig, CoordinationHub, NotificationError, NotificationId,
    PollError, PollableId, WaitMode,
};
use crate::epoch_clock::EpochTicker;
use crate::filesystem_watch::{
    FilesystemWatchBatchEvent, FilesystemWatchError, FilesystemWatchEventKind, FilesystemWatchHub,
    FilesystemWatchId, NativeFilesystemWatcherService,
};
use crate::lifecycle::wait_for_lifecycle_reply;
use crate::lifecycle::{PluginLifecycleEvent, PluginStartReason, PluginStopReason};
use crate::presentation::{
    ActorFrameDraw, FontIntent, FontStyle, FontWeight, PresentationError, PresentationEvent,
    PresentationHost, RenderHint, RenderToken, SessionCommandRequest, SessionEvent,
    SessionOperation, SessionSummary, TextLayoutOptions,
};
use crate::resource_limits::{
    GenerationByteBudget, GenerationBytePermit, GenerationStoreLimits, WasmResourceLimits,
};
use crate::runtime_io::{
    ChildStatus, PtySpawnOptions, RuntimeIoConfig, RuntimeIoError, RuntimeIoHub, StreamRead,
    TerminalProfile, TimerStatus,
};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    io::{self, Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{FileTypeExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::Duration,
};
use wasmtime::{
    AsContextMut, Config, Engine, Store,
    component::{Component, GuestTaskId, HasSelf, Linker, Resource},
};

static NEXT_REPLACE_TEMP: AtomicU64 = AtomicU64::new(1);

/// An actor-private worker identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(u64);

impl WorkerId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Realm-host feedback delivered only to a blessed init actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentChildEvent {
    Started { id: String, generation: u64 },
    Stopped { id: String },
    Failed { id: String, message: String },
}

/// Session-host app lifecycle feedback delivered only to a blessed init actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentAppEvent {
    Started { id: String, pane: u64 },
    Stopped { id: String },
    Failed { id: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentActorEvent {
    DispatcherReturned,
    /// One serialized presentation turn has returned from guest code. Hosts
    /// advance causal reduction only from this terminal observer record.
    PresentationTurnReturned {
        event_watermark: Option<u64>,
        render_token: Option<RenderToken>,
        presentation_events: u8,
    },
    DispatcherFailed {
        message: String,
    },
    WorkerStarted {
        worker: WorkerId,
    },
    WorkerFinished {
        worker: WorkerId,
        output: Vec<u8>,
    },
    WorkerFailed {
        worker: WorkerId,
        message: String,
    },
    WorkerCancelled {
        worker: WorkerId,
    },
    /// A worker has opened a special file nonblocking and is now waiting for
    /// readiness. This is host-side lifecycle evidence, not a guest message.
    WorkerIoPending {
        path: PathBuf,
    },
    NotificationDelivered {
        notification: NotificationId,
        generation: u64,
        token: u64,
    },
    CompletionDelivered(ActorCompletion),
    ActorStartRequested {
        id: String,
        path: String,
        capabilities: Vec<String>,
        presentation_scope: PresentationScope,
    },
    ActorStopRequested {
        id: String,
    },
    AppStartRequested {
        id: String,
        argv: Vec<String>,
    },
    SessionMessageRequested {
        backend_id: u64,
        session_id: u64,
        actor: String,
        body: Vec<u8>,
    },
    DaemonMessageRequested {
        backend_id: u64,
        actor: String,
        body: Vec<u8>,
    },
    LocalMessageRequested {
        actor: String,
        body: Vec<u8>,
    },
    PresentationMessageRequested {
        actor: String,
        body: Vec<u8>,
    },
    SessionCommandRequested(SessionCommandRequest),
    HostMessageRequested {
        payload: Vec<u8>,
    },
    Stopped,
}

/// Immutable presentation placement requested for a realm-init child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationScope {
    Global,
    Session { backend_id: u64, session_id: u64 },
}

#[derive(Debug)]
pub enum ComponentActorSendError<T> {
    Full(T),
    Closed(T),
    InvariantViolation,
}

#[derive(Debug)]
pub enum ComponentActorPresentationEnvelopeSendError {
    Full(Vec<(PresentationEvent, Option<u64>)>),
    Closed(Vec<(PresentationEvent, Option<u64>)>),
    InvalidSize(Vec<(PresentationEvent, Option<u64>)>),
    InvariantViolation,
}

impl<T> std::fmt::Display for ComponentActorSendError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Full(_) => "component actor command queue is full",
            Self::Closed(_) => "component actor command queue is closed",
            Self::InvariantViolation => {
                "component actor command queue returned a different command"
            }
        })
    }
}

impl<T: std::fmt::Debug> std::error::Error for ComponentActorSendError<T> {}

impl std::fmt::Display for ComponentActorPresentationEnvelopeSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Full(_) => "component actor command queue is full",
            Self::Closed(_) => "component actor command queue is closed",
            Self::InvalidSize(_) => "presentation envelope must contain 1 to 16 events",
            Self::InvariantViolation => {
                "component actor command queue returned a non-presentation command"
            }
        })
    }
}

impl std::error::Error for ComponentActorPresentationEnvelopeSendError {}

impl PresentationScope {
    #[must_use]
    pub const fn session(self) -> Option<(u64, u64)> {
        match self {
            Self::Global => None,
            Self::Session {
                backend_id,
                session_id,
            } => Some((backend_id, session_id)),
        }
    }
}

/// Cumulative delivery counters and the configured capacity for an actor's
/// global mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentActorMailboxStats {
    pub enqueued_messages: u64,
    pub dropped_messages: u64,
    pub capacity: usize,
}

#[derive(Clone)]
pub struct ComponentActorEventWaker(Arc<dyn Fn() + Send + Sync>);

impl ComponentActorEventWaker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(wake))
    }

    fn wake(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for ComponentActorEventWaker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ComponentActorEventWaker")
    }
}

#[derive(Clone)]
struct ComponentActorEventObserver {
    // Observer events are control and terminal records. Once admitted they
    // must remain observable even when the host is temporarily descheduled.
    sender: mpsc::Sender<ComponentActorEvent>,
    waker: Option<ComponentActorEventWaker>,
    dispatcher_returned_pending: Arc<AtomicBool>,
}

impl ComponentActorEventObserver {
    const TERMINAL_RESERVE: usize = 2;

    fn new(
        sender: mpsc::Sender<ComponentActorEvent>,
        waker: Option<ComponentActorEventWaker>,
    ) -> Self {
        Self {
            sender,
            waker,
            dispatcher_returned_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    fn try_send(
        &self,
        event: ComponentActorEvent,
    ) -> std::result::Result<(), mpsc::error::TrySendError<ComponentActorEvent>> {
        let terminal = matches!(
            event,
            ComponentActorEvent::DispatcherFailed { .. } | ComponentActorEvent::Stopped
        );
        if !terminal && self.sender.capacity() <= Self::TERMINAL_RESERVE {
            return Err(mpsc::error::TrySendError::Full(event));
        }
        let dispatcher_returned = matches!(event, ComponentActorEvent::DispatcherReturned);
        if dispatcher_returned
            && self
                .dispatcher_returned_pending
                .swap(true, Ordering::AcqRel)
        {
            return Ok(());
        }
        if let Err(error) = self.sender.try_send(event) {
            if dispatcher_returned {
                self.dispatcher_returned_pending
                    .store(false, Ordering::Release);
            }
            return Err(error);
        }
        if let Some(waker) = self.waker.as_ref() {
            waker.wake();
        }
        Ok(())
    }

    fn has_ordinary_capacity(&self, required: usize) -> bool {
        self.sender.capacity() >= required.saturating_add(Self::TERMINAL_RESERVE)
    }
}

#[derive(Debug, Clone)]
pub struct ComponentActorConfig {
    pub filesystem: Option<FilesystemGrant>,
    /// Daemon-owned native watcher registry shared across actor generations.
    pub filesystem_watcher_service: NativeFilesystemWatcherService,
    pub max_filesystem_watches: usize,
    pub max_filesystem_watch_events: usize,
    pub command_capacity: usize,
    /// Bounded queue for pane input and presentation lifecycle events.
    /// Consecutive pointer motion and resize state for one pane may coalesce;
    /// key, text, button, and focus transitions are never silently discarded.
    pub presentation_backlog: usize,
    pub mailbox_backlog: usize,
    pub max_workers: usize,
    pub max_outstanding_io: usize,
    pub max_completion_bytes: usize,
    /// Maximum payload for one worker whole-file atomic replacement.
    pub max_worker_replace_bytes: usize,
    pub max_notifications: usize,
    pub max_pollables: usize,
    pub max_wait_set: usize,
    /// Generation-owned process, PTY, socket, stream, and timer policy.
    pub runtime_io: RuntimeIoConfig,
    /// Executor for subprocess, stream, timer, and filesystem I/O. Hosts which
    /// run actor dispatch on a dedicated runtime set this to their general I/O
    /// runtime so guest services cannot contend with interactive actor turns.
    pub io_runtime: Option<tokio::runtime::Handle>,
    /// Presentation-local canvas, resource, and native text capability.
    pub presentation: Option<Arc<dyn PresentationHost>>,
    /// Admit realm child lifecycle commands. Hosts set this only for the one
    /// blessed init actor in a realm.
    pub actor_management: bool,
    /// Admit session-local Wayland app launch commands. Only a blessed
    /// surface-session init actor receives this capability.
    pub app_management: bool,
    /// Admit explicitly addressed presentation-to-session actor messages.
    /// Backend actors never receive this cross-placement capability.
    pub session_messaging: bool,
    /// Admit explicitly addressed presentation-to-daemon actor messages.
    /// Backend actors never receive this cross-placement capability.
    pub daemon_messaging: bool,
    /// Admit explicitly addressed session-to-presentation actor messages.
    /// Presentation actors never receive this cross-placement capability.
    pub presentation_messaging: bool,
    /// Wake the owning host loop when the actor emits an event that must be
    /// routed outside the actor runtime.
    pub event_waker: Option<ComponentActorEventWaker>,
    /// Admit typed session catalogue commands. Presentation clients set this
    /// for client-realm Component actors only.
    pub session_control: bool,
    /// Wall-clock cadence for guest interruption checks. Epoch yields keep the
    /// executor responsive; cross-call worker fairness uses `worker-yield`.
    pub epoch_tick_interval: Duration,
    /// Engine epochs between executor yields from guest code.
    pub epoch_yield_ticks: u64,
    /// Host and guest byte ceilings for one actor generation.
    pub limits: WasmResourceLimits,
}

impl Default for ComponentActorConfig {
    fn default() -> Self {
        Self {
            filesystem: None,
            filesystem_watcher_service: NativeFilesystemWatcherService::new(),
            max_filesystem_watches: 16,
            max_filesystem_watch_events: 4096,
            command_capacity: 64,
            presentation_backlog: 512,
            mailbox_backlog: 64,
            max_workers: 64,
            max_outstanding_io: 64,
            max_completion_bytes: 1024 * 1024,
            max_worker_replace_bytes: 8 * 1024 * 1024,
            max_notifications: 64,
            max_pollables: 256,
            max_wait_set: 64,
            runtime_io: RuntimeIoConfig::default(),
            io_runtime: None,
            presentation: None,
            actor_management: false,
            app_management: false,
            session_messaging: false,
            daemon_messaging: false,
            presentation_messaging: false,
            event_waker: None,
            session_control: false,
            epoch_tick_interval: Duration::from_millis(2),
            epoch_yield_ticks: 1,
            limits: WasmResourceLimits::default(),
        }
    }
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

fn start_completion_io(
    generation: u64,
    config: &ComponentActorConfig,
    runtime: &tokio::runtime::Handle,
) -> Result<(
    CompletionIo,
    mpsc::Receiver<ActorCompletion>,
    JoinHandle<()>,
)> {
    start_shared_completion_io_on(
        generation,
        &CompletionIoConfig {
            filesystem: config.filesystem.clone(),
            max_outstanding: config.max_outstanding_io,
            max_completion_bytes: config.max_completion_bytes,
        },
        runtime,
    )
}

fn worker_write_file(
    root: &Dir,
    writable: bool,
    path: PathBuf,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<u64> {
    ensure!(writable, "filesystem capability is read-only");
    let mut options = OpenOptions::new();
    // O_NONBLOCK makes open safe; reject every non-regular target before I/O.
    options.write(true).create(true).nonblock(true);
    let mut file = root
        .open_with(&path, &options)
        .with_context(|| format!("open {path:?} for worker write"))?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "worker writes require a regular file"
    );
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("seek {path:?} for worker write"))?;
    file.write_all(&bytes)
        .with_context(|| format!("write {path:?}"))?;
    u64::try_from(bytes.len()).context("convert write length")
}

fn worker_remove_file(root: &Dir, writable: bool, path: PathBuf) -> Result<()> {
    ensure!(writable, "filesystem capability is read-only");
    root.remove_file(&path)
        .with_context(|| format!("remove {path:?} from worker filesystem"))
}

fn read_worker_regular_file(
    root: &Dir,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    // O_NONBLOCK bounds special-file open behavior. The no-follow final
    // component plus descriptor metadata check makes the regular-file
    // decision apply to the object actually opened, without a path-stat race.
    options.read(true).nonblock(true).follow(FollowSymlinks::No);
    let mut file = root
        .open_with(&path, &options)
        .with_context(|| format!("open {path:?} for regular worker read"))?;
    ensure!(
        file.metadata()?.file_type().is_file(),
        "worker regular reads require a regular file"
    );
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("seek {path:?} for regular worker read"))?;
    let mut bytes = vec![0; max_bytes];
    let count = file
        .read(&mut bytes)
        .with_context(|| format!("read {path:?} as a regular file"))?;
    bytes.truncate(count);
    Ok(bytes)
}

fn worker_replace_file(root: &Dir, writable: bool, path: PathBuf, bytes: Vec<u8>) -> Result<u64> {
    ensure!(writable, "filesystem capability is read-only");
    ensure!(
        path.file_name().is_some(),
        "worker replacement requires a regular-file path"
    );

    let existing_permissions = match root.symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "worker replacement requires a regular-file destination"
            );
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {path:?} for worker replacement"));
        }
    };

    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let mut temporary = None;
    for _attempt in 0..32 {
        let nonce = NEXT_REPLACE_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".brood-replace-{}-{nonce}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).nonblock(true);
        match root.open_with(&candidate, &options) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create sibling temporary file for {path:?}"));
            }
        }
    }
    let (temporary_path, mut temporary_file) =
        temporary.context("could not allocate a sibling temporary file")?;

    let replacement = (|| {
        temporary_file
            .write_all(&bytes)
            .with_context(|| format!("write sibling temporary file for {path:?}"))?;
        if let Some(permissions) = existing_permissions {
            temporary_file
                .set_permissions(permissions)
                .with_context(|| format!("preserve permissions for {path:?}"))?;
        }
        temporary_file
            .sync_all()
            .with_context(|| format!("sync sibling temporary file for {path:?}"))?;
        drop(temporary_file);
        root.rename(&temporary_path, root, &path)
            .with_context(|| format!("atomically replace {path:?}"))?;
        Ok::<(), anyhow::Error>(())
    })();

    if replacement.is_err() {
        let _ = root.remove_file(&temporary_path);
    }
    replacement?;
    u64::try_from(bytes.len()).context("convert replacement length")
}

struct ActorStore {
    completion_io: CompletionIo,
    coordination: CoordinationHub,
    filesystem_watches: FilesystemWatchHub,
    runtime_io: RuntimeIoHub,
    actor_generation: u64,
    presentation: Option<Arc<dyn PresentationHost>>,
    worker_filesystem: Option<WorkerFilesystem>,
    worker_cancellations: WorkerCancellationRegistry,
    blocking_io: BlockingIoTracker,
    event_observer: ComponentActorEventObserver,
    shutdown_requested: Arc<AtomicBool>,
    limits: GenerationStoreLimits,
}

#[derive(Clone)]
struct WorkerFilesystem {
    root: Arc<Dir>,
    writable: bool,
    max_bytes: usize,
    max_replace_bytes: usize,
}

#[derive(Default)]
struct WorkerCancellationRegistry {
    calls: Mutex<HashMap<WorkerId, WorkerCallState>>,
}

impl WorkerCancellationRegistry {
    fn register(&self, worker: WorkerId, task: GuestTaskId) -> Result<()> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("component worker cancellation registry is poisoned"))?;
        let previous = calls.insert(
            worker,
            WorkerCallState {
                task,
                cancellation: Arc::new(WorkerCancellationToken::default()),
            },
        );
        drop(calls);
        ensure!(
            previous.is_none(),
            "component worker {} already has a registered guest task",
            worker.raw()
        );
        Ok(())
    }

    fn current(&self, stack: &[GuestTaskId]) -> Result<(WorkerId, Arc<WorkerCancellationToken>)> {
        self.calls
            .lock()
            .map_err(|_| anyhow::anyhow!("component worker cancellation registry is poisoned"))?
            .iter()
            .find(|(_worker, call)| stack.contains(&call.task))
            .map(|(worker, call)| (*worker, Arc::clone(&call.cancellation)))
            .context("worker-only host API called outside a registered worker task")
    }

    fn request(&self, worker: WorkerId) -> Result<bool> {
        let calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("component worker cancellation registry is poisoned"))?;
        let cancellation = calls
            .get(&worker)
            .map(|call| Arc::clone(&call.cancellation))
            .with_context(|| format!("component worker {} is not active", worker.raw()))?;
        drop(calls);
        Ok(cancellation.request())
    }

    fn remove(&self, worker: WorkerId) -> Result<()> {
        let removed = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("component worker cancellation registry is poisoned"))?
            .remove(&worker);
        ensure!(
            removed.is_some(),
            "component worker {} has no cancellation token",
            worker.raw()
        );
        Ok(())
    }
}

struct WorkerCallState {
    task: GuestTaskId,
    cancellation: Arc<WorkerCancellationToken>,
}

#[derive(Default)]
struct WorkerCancellationToken {
    requested: AtomicBool,
    notify: Notify,
}

#[derive(Clone, Default)]
struct BlockingIoTracker {
    inner: Arc<BlockingIoTrackerInner>,
}

#[derive(Default)]
struct BlockingIoTrackerInner {
    active: AtomicUsize,
    idle: Notify,
}

impl BlockingIoTracker {
    fn spawn<F, T>(&self, operation: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.inner.active.fetch_add(1, Ordering::AcqRel);
        let tracker = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = BlockingIoGuard(tracker);
            operation()
        })
    }

    async fn wait_idle(&self) {
        while self.inner.active.load(Ordering::Acquire) != 0 {
            self.inner.idle.notified().await;
        }
    }
}

struct BlockingIoGuard(BlockingIoTracker);

impl Drop for BlockingIoGuard {
    fn drop(&mut self) {
        if self.0.inner.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.inner.idle.notify_one();
        }
    }
}

impl WorkerCancellationToken {
    fn request(&self) -> bool {
        if self.requested.swap(true, Ordering::AcqRel) {
            return false;
        }
        // Each worker can be suspended in at most one host call. `notify_one`
        // retains a permit when cancellation races registration of that wait.
        self.notify.notify_one();
        true
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        while !self.is_requested() {
            self.notify.notified().await;
        }
    }
}

impl bindings::brood::plugin::host::Host for ActorStore {
    fn render_request(
        &mut self,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::RenderInterestError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| presentation.request_render(self.actor_generation))
            .map_err(map_component_render_interest_error))
    }

    fn render_set_subscription(
        &mut self,
        enabled: bool,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::RenderInterestError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.set_render_subscription(self.actor_generation, enabled)
            })
            .map_err(map_component_render_interest_error))
    }

    fn graphics_finish_actor_frame(
        &mut self,
        token: bindings::brood::plugin::host::RenderToken,
        draws: Vec<bindings::brood::plugin::host::ActorFrameDraw>,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::GraphicsError>>
    {
        let token = RenderToken(token.opaque);
        let draws = draws
            .into_iter()
            .map(|draw| ActorFrameDraw {
                pane: draw.pane,
                resize_epoch: draw.resize_epoch,
                state_watermark: draw.state_watermark,
                packet: draw.packet,
            })
            .collect();
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.finish_actor_frame(self.actor_generation, token, draws)
            })
            .map_err(map_component_presentation_error))
    }

    fn graphics_reuse_pane_content(
        &mut self,
        pane: u64,
        resize_epoch: u64,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::GraphicsError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.reuse_pane_content(self.actor_generation, pane, resize_epoch)
            })
            .map_err(map_component_presentation_error))
    }

    fn graphics_create_image_rgba8(
        &mut self,
        pane: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::GraphicsError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.create_image_rgba8(self.actor_generation, pane, width, height, pixels)
            })
            .map_err(map_component_presentation_error))
    }

    fn graphics_layout_text(
        &mut self,
        pane: u64,
        text: String,
        options: bindings::brood::plugin::host::TextLayoutOptions,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::TextLayoutResult,
            bindings::brood::plugin::host::GraphicsError,
        >,
    > {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.layout_text(
                    self.actor_generation,
                    pane,
                    text,
                    TextLayoutOptions {
                        font_intent: match options.font_intent {
                            bindings::brood::plugin::host::FontIntent::Ui => FontIntent::Ui,
                            bindings::brood::plugin::host::FontIntent::Monospace => {
                                FontIntent::Monospace
                            }
                        },
                        font_weight: match options.font_weight {
                            bindings::brood::plugin::host::FontWeight::Normal => FontWeight::Normal,
                            bindings::brood::plugin::host::FontWeight::Bold => FontWeight::Bold,
                        },
                        font_style: match options.font_style {
                            bindings::brood::plugin::host::FontStyle::Normal => FontStyle::Normal,
                            bindings::brood::plugin::host::FontStyle::Italic => FontStyle::Italic,
                        },
                        font_size: options.font_size,
                        max_width: options.max_width,
                        line_height: options.line_height,
                        color: [options.red, options.green, options.blue, options.alpha],
                    },
                )
            })
            .map(|layout| bindings::brood::plugin::host::TextLayoutResult {
                handle: layout.handle,
                width: layout.width,
                height: layout.height,
                first_baseline: layout.first_baseline,
                line_count: layout.line_count,
                cell_width: layout.cell_width,
                line_height: layout.line_height,
            })
            .map_err(map_component_presentation_error))
    }

    fn graphics_drop_resource(
        &mut self,
        pane: u64,
        resource: u64,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::GraphicsError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.drop_resource(self.actor_generation, pane, resource)
            })
            .map_err(map_component_presentation_error))
    }

    fn subprocess_spawn(
        &mut self,
        command: String,
        arguments: Vec<String>,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::ProcessIo,
            bindings::brood::plugin::host::RuntimeIoError,
        >,
    > {
        Ok(self
            .runtime_io
            .spawn_process(&command, &arguments)
            .map(|process| bindings::brood::plugin::host::ProcessIo {
                child: process.child.raw(),
                stdin: process.stdin.raw(),
                stdout: process.stdout.raw(),
                stderr: process.stderr.raw(),
                resolved_command: process
                    .child
                    .resolved_command()
                    .to_string_lossy()
                    .into_owned(),
            })
            .map_err(map_component_runtime_io_error))
    }

    fn pty_spawn(
        &mut self,
        command: String,
        arguments: Vec<String>,
        options: bindings::brood::plugin::host::PtySpawnOptions,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::PtyIo,
            bindings::brood::plugin::host::RuntimeIoError,
        >,
    > {
        Ok(self
            .runtime_io
            .spawn_pty(
                &command,
                &arguments,
                PtySpawnOptions {
                    rows: options.rows,
                    cols: options.cols,
                    terminal_profile: match options.terminal_profile {
                        bindings::brood::plugin::host::TerminalProfile::Xterm256color => {
                            TerminalProfile::Xterm256Color
                        }
                    },
                },
            )
            .map(|pty| bindings::brood::plugin::host::PtyIo {
                child: pty.child.raw(),
                terminal: pty.terminal.raw(),
                resolved_command: pty.child.resolved_command().to_string_lossy().into_owned(),
            })
            .map_err(map_component_runtime_io_error))
    }

    fn pty_resize(
        &mut self,
        child: u64,
        rows: u16,
        cols: u16,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .resize_pty(child, rows, cols)
            .map_err(map_component_runtime_io_error))
    }

    fn stream_notification(
        &mut self,
        handle: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .stream(handle)
            .map(|stream| stream.notification().raw())
            .map_err(map_component_runtime_io_error))
    }

    fn stream_read(
        &mut self,
        handle: u64,
        max_bytes: u32,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::StreamReadResult,
            bindings::brood::plugin::host::RuntimeIoError,
        >,
    > {
        Ok(self
            .runtime_io
            .stream(handle)
            .and_then(|stream| stream.try_read(max_bytes as usize))
            .map(|read| match read {
                StreamRead::Data(bytes) => {
                    bindings::brood::plugin::host::StreamReadResult::Data(bytes)
                }
                StreamRead::WouldBlock => {
                    bindings::brood::plugin::host::StreamReadResult::WouldBlock
                }
                StreamRead::Eof => bindings::brood::plugin::host::StreamReadResult::Eof,
            })
            .map_err(map_component_runtime_io_error))
    }

    fn stream_write(
        &mut self,
        handle: u64,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<u32, bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .stream(handle)
            .and_then(|stream| stream.try_write(&bytes))
            .and_then(|count| {
                u32::try_from(count).map_err(|_overflow| RuntimeIoError::InvalidArgument)
            })
            .map_err(map_component_runtime_io_error))
    }

    fn stream_close_write(
        &mut self,
        handle: u64,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .stream(handle)
            .and_then(|stream| stream.close_write())
            .map_err(map_component_runtime_io_error))
    }

    fn child_notification(
        &mut self,
        child: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .child(child)
            .map(|child| child.notification().raw())
            .map_err(map_component_runtime_io_error))
    }

    fn child_status(
        &mut self,
        child: u64,
    ) -> wasmtime::Result<
        std::result::Result<
            (bindings::brood::plugin::host::ChildState, Option<i32>),
            bindings::brood::plugin::host::RuntimeIoError,
        >,
    > {
        Ok(self
            .runtime_io
            .child(child)
            .and_then(|child| child.status())
            .map(|status| match status {
                ChildStatus::Running => (bindings::brood::plugin::host::ChildState::Running, None),
                ChildStatus::Exited(Some(code)) => (
                    bindings::brood::plugin::host::ChildState::Exited,
                    Some(code),
                ),
                ChildStatus::Exited(None) => {
                    (bindings::brood::plugin::host::ChildState::Signalled, None)
                }
            })
            .map_err(map_component_runtime_io_error))
    }

    fn timer_create(
        &mut self,
        delay_ms: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .create_timer(Duration::from_millis(delay_ms))
            .map(|timer| timer.raw())
            .map_err(map_component_runtime_io_error))
    }

    fn timer_notification(
        &mut self,
        timer: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::RuntimeIoError>>
    {
        Ok(self
            .runtime_io
            .timer(timer)
            .map(|timer| timer.notification().raw())
            .map_err(map_component_runtime_io_error))
    }

    fn timer_status(
        &mut self,
        timer: u64,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::TimerState,
            bindings::brood::plugin::host::RuntimeIoError,
        >,
    > {
        Ok(self
            .runtime_io
            .timer(timer)
            .and_then(|timer| timer.status())
            .map(|status| match status {
                TimerStatus::Pending => bindings::brood::plugin::host::TimerState::Pending,
                TimerStatus::Expired => bindings::brood::plugin::host::TimerState::Expired,
            })
            .map_err(map_component_runtime_io_error))
    }

    fn filesystem_watch_create(
        &mut self,
        path: String,
        recursive: bool,
    ) -> wasmtime::Result<
        std::result::Result<
            Resource<bindings::brood::plugin::host::FilesystemWatch>,
            bindings::brood::plugin::host::FilesystemWatchError,
        >,
    > {
        Ok(self
            .filesystem_watches
            .create(Path::new(&path), recursive)
            .and_then(|(watch, _notification)| {
                u32::try_from(watch.raw())
                    .map(Resource::new_own)
                    .map_err(|_error| FilesystemWatchError::Limit)
            })
            .map_err(map_component_filesystem_watch_error))
    }

    fn submit_read(
        &mut self,
        path: String,
        offset: u64,
        max_bytes: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::SubmitError>>
    {
        let max_bytes = usize::try_from(max_bytes)
            .map_err(|_| wasmtime::Error::msg("completion read size does not fit usize"))?;
        Ok(self
            .completion_io
            .submit_read(path, offset, max_bytes)
            .map(OperationId::sequence)
            .map_err(map_submit_error))
    }

    fn submit_write(
        &mut self,
        path: String,
        offset: u64,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::SubmitError>>
    {
        Ok(self
            .completion_io
            .submit_write(path, offset, bytes)
            .map(OperationId::sequence)
            .map_err(map_submit_error))
    }

    fn cancel(&mut self, operation: u64) -> wasmtime::Result<bool> {
        Ok(self
            .completion_io
            .cancel(OperationId::new(self.completion_io.generation(), operation)))
    }

    fn notification_create(
        &mut self,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::NotificationError>>
    {
        Ok(self
            .coordination
            .create_notification()
            .map(NotificationId::raw)
            .map_err(map_component_notification_error))
    }

    fn notification_drop(
        &mut self,
        notification: u64,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::NotificationError>>
    {
        Ok(self
            .coordination
            .drop_notification(NotificationId::from_raw(notification))
            .map_err(map_component_notification_error))
    }

    fn notification_signal(
        &mut self,
        notification: u64,
        published_generation: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::NotificationError>>
    {
        Ok(self
            .coordination
            .signal(NotificationId::from_raw(notification), published_generation)
            .map_err(map_component_notification_error))
    }

    fn notification_watch_actor(
        &mut self,
        notification: u64,
        observed_generation: u64,
        token: u64,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::WatchResult,
            bindings::brood::plugin::host::NotificationError,
        >,
    > {
        Ok(self
            .coordination
            .watch_actor(
                NotificationId::from_raw(notification),
                observed_generation,
                token,
            )
            .map(|result| match result {
                ActorWatchResult::Armed => bindings::brood::plugin::host::WatchResult::Armed,
                ActorWatchResult::Queued { generation } => {
                    bindings::brood::plugin::host::WatchResult::Queued(generation)
                }
            })
            .map_err(map_component_notification_error))
    }

    fn notification_unwatch_actor(
        &mut self,
        notification: u64,
    ) -> wasmtime::Result<std::result::Result<bool, bindings::brood::plugin::host::NotificationError>>
    {
        Ok(self
            .coordination
            .unwatch_actor(NotificationId::from_raw(notification))
            .map_err(map_component_notification_error))
    }

    fn notification_pollable(
        &mut self,
        notification: u64,
        observed_generation: u64,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::WaitError>> {
        Ok(self
            .coordination
            .notification_pollable(NotificationId::from_raw(notification), observed_generation)
            .map(PollableId::raw)
            .map_err(map_component_poll_error))
    }

    fn pollable_drop(
        &mut self,
        pollable: u64,
    ) -> wasmtime::Result<std::result::Result<(), bindings::brood::plugin::host::WaitError>> {
        Ok(self
            .coordination
            .drop_pollable(PollableId::from_raw(pollable))
            .map_err(map_component_poll_error))
    }

    fn pollable_try(
        &mut self,
        pollables: Vec<u64>,
    ) -> wasmtime::Result<
        std::result::Result<
            Vec<bindings::brood::plugin::host::ReadyPollable>,
            bindings::brood::plugin::host::WaitError,
        >,
    > {
        let pollables = pollables
            .into_iter()
            .map(PollableId::from_raw)
            .collect::<Vec<_>>();
        Ok(self
            .coordination
            .try_poll(&pollables)
            .map(|ready| ready.into_iter().map(component_ready_pollable).collect())
            .map_err(map_component_poll_error))
    }
}

impl bindings::brood::plugin::host::HostFilesystemWatch for ActorStore {
    fn notification(
        &mut self,
        watch: Resource<bindings::brood::plugin::host::FilesystemWatch>,
    ) -> wasmtime::Result<
        std::result::Result<u64, bindings::brood::plugin::host::FilesystemWatchError>,
    > {
        Ok(self
            .filesystem_watches
            .notification(FilesystemWatchId::from_raw(u64::from(watch.rep())))
            .map(NotificationId::raw)
            .map_err(map_component_filesystem_watch_error))
    }

    fn read(
        &mut self,
        watch: Resource<bindings::brood::plugin::host::FilesystemWatch>,
    ) -> wasmtime::Result<
        std::result::Result<
            bindings::brood::plugin::host::FilesystemWatchBatch,
            bindings::brood::plugin::host::FilesystemWatchError,
        >,
    > {
        Ok(self
            .filesystem_watches
            .read_typed(FilesystemWatchId::from_raw(u64::from(watch.rep())))
            .map(
                |(generation, events)| bindings::brood::plugin::host::FilesystemWatchBatch {
                    generation,
                    events: events
                        .into_iter()
                        .map(component_filesystem_watch_event)
                        .collect(),
                },
            )
            .map_err(map_component_filesystem_watch_error))
    }

    fn drop(
        &mut self,
        watch: Resource<bindings::brood::plugin::host::FilesystemWatch>,
    ) -> wasmtime::Result<()> {
        self.filesystem_watches
            .drop_watch(FilesystemWatchId::from_raw(u64::from(watch.rep())))
            .map_err(|error| wasmtime::Error::msg(error.to_string()))
    }
}

impl bindings::brood::plugin::host::HostWithStore<ActorStore> for HasSelf<ActorStore> {
    fn stream_drop(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        handle: u64,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<(), bindings::brood::plugin::host::RuntimeIoError>,
        >,
    > + Send {
        let runtime_io = accessor.with(|mut access| access.get().runtime_io.clone());
        async move {
            Ok(runtime_io
                .drop_stream(handle)
                .await
                .map_err(map_component_runtime_io_error))
        }
    }

    fn child_drop(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        child: u64,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<(), bindings::brood::plugin::host::RuntimeIoError>,
        >,
    > + Send {
        let runtime_io = accessor.with(|mut access| access.get().runtime_io.clone());
        async move {
            Ok(runtime_io
                .drop_child(child)
                .await
                .map_err(map_component_runtime_io_error))
        }
    }

    fn timer_drop(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        timer: u64,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<(), bindings::brood::plugin::host::RuntimeIoError>,
        >,
    > + Send {
        let runtime_io = accessor.with(|mut access| access.get().runtime_io.clone());
        async move {
            Ok(runtime_io
                .drop_timer(timer)
                .await
                .map_err(map_component_runtime_io_error))
        }
    }

    fn worker_loopback_connect(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        address: String,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<u64, bindings::brood::plugin::host::RuntimeIoError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let runtime_io = accessor.with(|mut access| access.get().runtime_io.clone());
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(
                    bindings::brood::plugin::host::RuntimeIoError::ActorStopped,
                ));
            }
            let address = match address.parse::<SocketAddr>() {
                Ok(address) => address,
                Err(_error) => {
                    return Ok(Err(
                        bindings::brood::plugin::host::RuntimeIoError::InvalidArgument,
                    ));
                }
            };
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(RuntimeIoError::StaleHandle),
                result = runtime_io.connect_loopback(address) => result,
            };
            Ok(result
                .map(|stream| stream.raw())
                .map_err(map_component_runtime_io_error))
        }
    }

    fn worker_wait_any(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        pollables: Vec<u64>,
        timeout_ms: Option<u64>,
        mode: bindings::brood::plugin::host::WaitMode,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<
                bindings::brood::plugin::host::ReadyPollable,
                bindings::brood::plugin::host::WaitError,
            >,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let coordination = accessor.with(|mut access| access.get().coordination.clone());
        async move {
            let (_worker, cancellation) = current?;
            let pollables = pollables
                .into_iter()
                .map(PollableId::from_raw)
                .collect::<Vec<_>>();
            let mode = match mode {
                bindings::brood::plugin::host::WaitMode::Biased => WaitMode::Biased,
                bindings::brood::plugin::host::WaitMode::Fair => WaitMode::Fair,
            };
            let wait =
                coordination.wait_any(&pollables, timeout_ms.map(Duration::from_millis), mode);
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(PollError::Cancelled),
                result = wait => result,
            };
            Ok(result
                .map(component_ready_pollable)
                .map_err(map_component_poll_error))
        }
    }

    async fn worker_yield(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
    ) -> wasmtime::Result<bool> {
        let (_worker, cancellation) = current_component_worker(accessor)?;
        tokio::task::yield_now().await;
        Ok(cancellation.is_requested())
    }

    fn worker_read_file(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        path: String,
        offset: u64,
        max_bytes: u64,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<Vec<u8>, bindings::brood::plugin::host::SubmitError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let (filesystem, event_observer, blocking_io) = accessor.with(|mut access| {
            let store = access.get();
            (
                store.worker_filesystem.clone(),
                store.event_observer.clone(),
                store.blocking_io.clone(),
            )
        });
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            let Some(filesystem) = filesystem else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            };
            let Ok(max_bytes) = usize::try_from(max_bytes) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            if max_bytes > filesystem.max_bytes {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            }
            let Ok(path) = validate_capability_path(Path::new(&path)) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            let result = worker_read_file(
                filesystem,
                path,
                offset,
                max_bytes,
                Some(event_observer),
                blocking_io,
                Some(Arc::clone(&cancellation)),
            )
            .await;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            match result {
                Ok(bytes) => Ok(Ok(bytes)),
                Err(_) => Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                )),
            }
        }
    }

    fn worker_read_regular_file(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        path: String,
        offset: u64,
        max_bytes: u64,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<Vec<u8>, bindings::brood::plugin::host::SubmitError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let (filesystem, blocking_io) = accessor.with(|mut access| {
            let store = access.get();
            (store.worker_filesystem.clone(), store.blocking_io.clone())
        });
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            let Some(filesystem) = filesystem else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            };
            let Ok(max_bytes) = usize::try_from(max_bytes) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            if max_bytes > filesystem.max_bytes {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            }
            let Ok(path) = validate_capability_path(Path::new(&path)) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            let result = blocking_io
                .spawn(move || {
                    read_worker_regular_file(filesystem.root.as_ref(), path, offset, max_bytes)
                })
                .await;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            match result {
                Ok(Ok(bytes)) => Ok(Ok(bytes)),
                Ok(Err(_)) | Err(_) => Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                )),
            }
        }
    }

    fn worker_write_file(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        path: String,
        offset: u64,
        bytes: Vec<u8>,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<u64, bindings::brood::plugin::host::SubmitError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let (filesystem, blocking_io) = accessor.with(|mut access| {
            let store = access.get();
            (store.worker_filesystem.clone(), store.blocking_io.clone())
        });
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            let Some(filesystem) = filesystem else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            };
            if !filesystem.writable {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            }
            if bytes.len() > filesystem.max_bytes {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            }
            let Ok(path) = validate_capability_path(Path::new(&path)) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            let result = blocking_io
                .spawn(move || {
                    worker_write_file(
                        filesystem.root.as_ref(),
                        filesystem.writable,
                        path,
                        offset,
                        bytes,
                    )
                })
                .await;
            // A regular-file write may already have been delegated to a
            // blocking thread. Join it before reporting cancellation so no
            // worker-owned side effect can occur after the terminal event.
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            match result {
                Ok(Ok(bytes_written)) => Ok(Ok(bytes_written)),
                Ok(Err(_)) | Err(_) => Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                )),
            }
        }
    }

    fn worker_replace_file(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        path: String,
        bytes: Vec<u8>,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<u64, bindings::brood::plugin::host::SubmitError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let (filesystem, blocking_io) = accessor.with(|mut access| {
            let store = access.get();
            (store.worker_filesystem.clone(), store.blocking_io.clone())
        });
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            let Some(filesystem) = filesystem else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            };
            if !filesystem.writable {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            }
            if bytes.len() > filesystem.max_replace_bytes {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            }
            let Ok(path) = validate_capability_path(Path::new(&path)) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            let result = blocking_io
                .spawn(move || {
                    worker_replace_file(filesystem.root.as_ref(), filesystem.writable, path, bytes)
                })
                .await;
            // The replacement may already have reached the blocking pool.
            // Join it before reporting cancellation so no worker-owned side
            // effect can occur after the terminal event.
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            match result {
                Ok(Ok(bytes_written)) => Ok(Ok(bytes_written)),
                Ok(Err(_)) | Err(_) => Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                )),
            }
        }
    }

    fn worker_remove_file(
        accessor: &wasmtime::component::Accessor<ActorStore, Self>,
        path: String,
    ) -> impl Future<
        Output = wasmtime::Result<
            std::result::Result<(), bindings::brood::plugin::host::SubmitError>,
        >,
    > + Send {
        let current = current_component_worker(accessor);
        let (filesystem, blocking_io) = accessor.with(|mut access| {
            let store = access.get();
            (store.worker_filesystem.clone(), store.blocking_io.clone())
        });
        async move {
            let (_worker, cancellation) = current?;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            let Some(filesystem) = filesystem else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            };
            if !filesystem.writable {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                ));
            }
            let Ok(path) = validate_capability_path(Path::new(&path)) else {
                return Ok(Err(
                    bindings::brood::plugin::host::SubmitError::InvalidArgument,
                ));
            };
            let result = blocking_io
                .spawn(move || {
                    worker_remove_file(filesystem.root.as_ref(), filesystem.writable, path)
                })
                .await;
            if cancellation.is_requested() {
                return Ok(Err(bindings::brood::plugin::host::SubmitError::Cancelled));
            }
            match result {
                Ok(Ok(())) => Ok(Ok(())),
                Ok(Err(_)) | Err(_) => Ok(Err(
                    bindings::brood::plugin::host::SubmitError::NotPermitted,
                )),
            }
        }
    }
}

fn current_component_worker(
    accessor: &wasmtime::component::Accessor<ActorStore, HasSelf<ActorStore>>,
) -> wasmtime::Result<(WorkerId, Arc<WorkerCancellationToken>)> {
    accessor
        .with(|mut access| {
            let stack = access
                .as_context_mut()
                .async_call_stack()
                .collect::<Vec<_>>();
            access.get().worker_cancellations.current(&stack)
        })
        .map_err(|error| wasmtime::Error::msg(error.to_string()))
}

async fn worker_read_file(
    filesystem: WorkerFilesystem,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
    pending_observer: Option<ComponentActorEventObserver>,
    blocking_io: BlockingIoTracker,
    cancellation: Option<Arc<WorkerCancellationToken>>,
) -> Result<Vec<u8>> {
    let pending_path = path.clone();
    let prepared = blocking_io
        .spawn(move || prepare_worker_read(filesystem, path, offset, max_bytes))
        .await
        .context("join worker filesystem preparation")??;
    if cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.is_requested())
    {
        anyhow::bail!("component worker cancelled during filesystem preparation");
    }
    let (file, max_bytes) = match prepared {
        PreparedWorkerRead::Complete(bytes) => return Ok(bytes),
        PreparedWorkerRead::Async(file, max_bytes) => (file, max_bytes),
    };

    let async_file = tokio::io::unix::AsyncFd::new(file)?;
    if let Some(observer) = pending_observer {
        let _ = observer.try_send(ComponentActorEvent::WorkerIoPending { path: pending_path });
    }
    let mut bytes = vec![0; max_bytes];
    loop {
        let mut guard = if let Some(cancellation) = cancellation.as_ref() {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    anyhow::bail!("component worker cancelled while waiting for file readiness");
                }
                guard = async_file.readable() => guard?,
            }
        } else {
            async_file.readable().await?
        };
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

enum PreparedWorkerRead {
    Complete(Vec<u8>),
    Async(std::fs::File, usize),
}

fn prepare_worker_read(
    filesystem: WorkerFilesystem,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
) -> Result<PreparedWorkerRead> {
    let mut options = OpenOptions::new();
    options.read(true).nonblock(true);
    // Opening both ends prevents a FIFO from reporting EOF before an external
    // producer connects. This is only permitted for a writable grant.
    if filesystem.writable {
        options.write(true);
    }
    let mut file = filesystem
        .root
        .open_with(&path, &options)
        .with_context(|| format!("open {:?} for worker read", path))?;
    let file_type = file.metadata()?.file_type();
    if !file_type.is_fifo() && !file_type.is_socket() {
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; max_bytes];
        let count = file.read(&mut bytes)?;
        bytes.truncate(count);
        return Ok(PreparedWorkerRead::Complete(bytes));
    }
    Ok(PreparedWorkerRead::Async(file.into_std(), max_bytes))
}

const fn map_submit_error(error: CompletionIoError) -> bindings::brood::plugin::host::SubmitError {
    use bindings::brood::plugin::host::SubmitError;
    match error {
        CompletionIoError::ActorStopped => SubmitError::ActorStopped,
        CompletionIoError::LimitReached => SubmitError::LimitReached,
        CompletionIoError::BacklogFull => SubmitError::BacklogFull,
        CompletionIoError::NotPermitted => SubmitError::NotPermitted,
        CompletionIoError::InvalidArgument => SubmitError::InvalidArgument,
    }
}

const fn map_component_notification_error(
    error: NotificationError,
) -> bindings::brood::plugin::host::NotificationError {
    use bindings::brood::plugin::host::NotificationError as AbiError;
    match error {
        NotificationError::Stopped => AbiError::ActorStopped,
        NotificationError::LimitReached => AbiError::LimitReached,
        NotificationError::BacklogFull => AbiError::BacklogFull,
        NotificationError::InvalidHandle => AbiError::InvalidHandle,
        NotificationError::InvalidGeneration => AbiError::InvalidGeneration,
        NotificationError::AlreadyWatched => AbiError::AlreadyWatched,
    }
}

fn map_component_filesystem_watch_error(
    error: FilesystemWatchError,
) -> bindings::brood::plugin::host::FilesystemWatchError {
    use bindings::brood::plugin::host::FilesystemWatchError as AbiError;
    match error {
        FilesystemWatchError::Stopped => AbiError::ActorStopped,
        FilesystemWatchError::Limit => AbiError::LimitReached,
        FilesystemWatchError::Invalid => AbiError::InvalidHandle,
        FilesystemWatchError::NotPermitted => AbiError::NotPermitted,
        FilesystemWatchError::NotReady => AbiError::NotReady,
        FilesystemWatchError::Backend(_) | FilesystemWatchError::Unavailable => {
            AbiError::BackendUnavailable
        }
    }
}

fn component_filesystem_watch_event(
    event: FilesystemWatchBatchEvent,
) -> bindings::brood::plugin::host::FilesystemEvent {
    use bindings::brood::plugin::host::FilesystemEventKind as AbiKind;
    let kind = match event.kind {
        FilesystemWatchEventKind::Created => AbiKind::Created,
        FilesystemWatchEventKind::Modified => AbiKind::Modified,
        FilesystemWatchEventKind::Removed => AbiKind::Removed,
        FilesystemWatchEventKind::Changed => AbiKind::Changed,
        FilesystemWatchEventKind::Rescan => AbiKind::Rescan,
    };
    bindings::brood::plugin::host::FilesystemEvent {
        kind,
        path: event.path,
    }
}

const fn map_component_poll_error(error: PollError) -> bindings::brood::plugin::host::WaitError {
    use bindings::brood::plugin::host::WaitError as AbiError;
    match error {
        PollError::Stopped => AbiError::ActorStopped,
        PollError::LimitReached => AbiError::LimitReached,
        PollError::InvalidPollable => AbiError::InvalidPollable,
        PollError::DuplicatePollable => AbiError::DuplicatePollable,
        PollError::EmptyWaitSet => AbiError::EmptyWaitSet,
        PollError::TimedOut => AbiError::TimedOut,
        PollError::Cancelled => AbiError::Cancelled,
    }
}

const fn map_component_runtime_io_error(
    error: RuntimeIoError,
) -> bindings::brood::plugin::host::RuntimeIoError {
    use bindings::brood::plugin::host::RuntimeIoError as AbiError;
    match error {
        RuntimeIoError::StaleHandle => AbiError::InvalidHandle,
        RuntimeIoError::LimitReached => AbiError::LimitReached,
        RuntimeIoError::NotPermitted => AbiError::NotPermitted,
        RuntimeIoError::InvalidArgument => AbiError::InvalidArgument,
        RuntimeIoError::HostFailure => AbiError::HostFailure,
        RuntimeIoError::WouldBlock => AbiError::WouldBlock,
        RuntimeIoError::Closed => AbiError::Closed,
    }
}

const fn map_component_presentation_error(
    error: PresentationError,
) -> bindings::brood::plugin::host::GraphicsError {
    use bindings::brood::plugin::host::GraphicsError as AbiError;
    match error {
        PresentationError::Unavailable | PresentationError::ActorStopped => AbiError::Unavailable,
        PresentationError::InvalidPane => AbiError::InvalidPane,
        PresentationError::InvalidToken => AbiError::InvalidToken,
        PresentationError::DuplicatePane => AbiError::DuplicatePane,
        PresentationError::StaleResizeEpoch => AbiError::StaleResizeEpoch,
        PresentationError::InvalidArgument => AbiError::InvalidArgument,
        PresentationError::LimitReached => AbiError::LimitReached,
        PresentationError::InvalidPacket => AbiError::InvalidPacket,
        PresentationError::InvalidResource => AbiError::InvalidResource,
        PresentationError::ResourceInUse => AbiError::ResourceInUse,
        PresentationError::HostFailure => AbiError::HostFailure,
    }
}

const fn map_component_render_interest_error(
    error: PresentationError,
) -> bindings::brood::plugin::host::RenderInterestError {
    use bindings::brood::plugin::host::RenderInterestError as AbiError;
    match error {
        PresentationError::Unavailable => AbiError::Unavailable,
        PresentationError::ActorStopped => AbiError::ActorStopped,
        PresentationError::InvalidArgument
        | PresentationError::InvalidPane
        | PresentationError::InvalidToken
        | PresentationError::DuplicatePane
        | PresentationError::StaleResizeEpoch
        | PresentationError::InvalidPacket
        | PresentationError::InvalidResource
        | PresentationError::ResourceInUse
        | PresentationError::LimitReached => AbiError::InvalidArgument,
        PresentationError::HostFailure => AbiError::HostFailure,
    }
}

fn component_ready_pollable(
    ready: crate::coordination::ReadyPollable,
) -> bindings::brood::plugin::host::ReadyPollable {
    bindings::brood::plugin::host::ReadyPollable {
        index: u32::try_from(ready.index).unwrap_or(u32::MAX),
        pollable: ready.pollable.raw(),
        generation: ready.generation,
    }
}

#[derive(Debug)]
enum RuntimeCommand {
    Mailbox {
        sender: String,
        body: Vec<u8>,
        permit: OwnedSemaphorePermit,
        byte_permit: GenerationBytePermit,
        profile_id: u64,
    },
    SpawnWorker {
        worker: WorkerId,
        payload: Vec<u8>,
        byte_permit: GenerationBytePermit,
    },
    CancelWorker(WorkerId),
    Lifecycle {
        event: PluginLifecycleEvent,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Child(ComponentChildEvent),
    App(ComponentAppEvent),
    Presentation {
        events: Vec<(PresentationEvent, Option<u64>)>,
    },
    Session(SessionEvent),
    Shutdown,
}

/// Handle for one live component actor.
///
/// [`Self::shutdown`] joins the runtime and I/O supervisor and waits for
/// tracked blocking I/O. [`Drop`] aborts the owned async handles as a fallback;
/// blocking-pool work which has already started cannot be preempted.
pub struct ComponentActor {
    kernel: ActorRuntimeKernel,
    completion_io: CompletionIo,
    commands: mpsc::Sender<RuntimeCommand>,
    mailbox_slots: Arc<Semaphore>,
    mailbox_capacity: usize,
    mailbox_enqueued: AtomicU64,
    mailbox_dropped: AtomicU64,
    events: mpsc::Receiver<ComponentActorEvent>,
    completions: mpsc::Receiver<ActorCompletion>,
    next_worker: AtomicU64,
    runtime_task: Option<JoinHandle<Result<()>>>,
    io_task: Option<JoinHandle<()>>,
    epoch_ticker: Option<EpochTicker>,
    shutdown_requested: Arc<AtomicBool>,
    limits: WasmResourceLimits,
    observer_result_bytes: Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
    dispatcher_returned_pending: Arc<AtomicBool>,
}

impl ComponentActor {
    /// Load and start a Component Model actor.
    pub async fn load(path: &Path, config: ComponentActorConfig) -> Result<Self> {
        ensure!(
            config.command_capacity > 0,
            "command capacity must be non-zero"
        );
        validate_presentation_backlog(config.presentation_backlog)?;
        ensure!(
            config.mailbox_backlog > 0,
            "mailbox backlog must be non-zero"
        );
        ensure!(config.max_workers > 0, "worker limit must be non-zero");
        ensure!(
            config.max_filesystem_watches > 0,
            "filesystem watch limit must be non-zero"
        );
        ensure!(
            config.max_filesystem_watch_events > 0,
            "filesystem watch event limit must be non-zero"
        );
        ensure!(
            config.max_worker_replace_bytes > 0,
            "worker replacement byte limit must be non-zero"
        );
        ensure!(
            config.max_notifications > 0,
            "notification limit must be non-zero"
        );
        ensure!(config.max_pollables > 0, "pollable limit must be non-zero");
        ensure!(
            config.max_wait_set > 0 && config.max_wait_set <= config.max_pollables,
            "wait-set limit must be non-zero and no greater than the pollable limit"
        );
        ensure!(
            !config.epoch_tick_interval.is_zero(),
            "epoch tick interval must be non-zero"
        );
        ensure!(config.epoch_yield_ticks > 0, "epoch yield must be non-zero");
        config.limits.validate()?;

        let generation = allocate_generation()
            .map_err(|_exhausted| anyhow::anyhow!("actor generation allocator is exhausted"))?;
        let (coordination, notification_rx) = CoordinationHub::new(
            generation,
            CoordinationConfig {
                max_notifications: config.max_notifications,
                max_pollables: config.max_pollables,
                max_wait_set: config.max_wait_set,
            },
        )
        .map_err(|error| anyhow::anyhow!("create component coordination hub: {error}"))?;
        let filesystem_watches = FilesystemWatchHub::new_with_service(
            config.filesystem_watcher_service.clone(),
            config.filesystem.as_ref(),
            coordination.clone(),
            config.max_filesystem_watches,
            config.max_filesystem_watch_events,
        )
        .map_err(|error| anyhow::anyhow!("create component filesystem watch hub: {error}"))?;
        let io_runtime = config
            .io_runtime
            .clone()
            .unwrap_or_else(tokio::runtime::Handle::current);
        let runtime_io = RuntimeIoHub::new_on(
            generation,
            config.runtime_io.clone(),
            coordination.clone(),
            io_runtime.clone(),
        )
        .map_err(|error| anyhow::anyhow!("create component runtime I/O hub: {error}"))?;
        let kernel = ActorRuntimeKernel::new(
            generation,
            coordination.clone(),
            filesystem_watches.clone(),
            runtime_io.clone(),
            config.limits.generation_host_bytes,
        );

        let engine = component_engine()?;
        let epoch_ticker = EpochTicker::start(engine.clone(), config.epoch_tick_interval)?;
        let component = Component::from_file(&engine, path)
            .map_err(|error| anyhow::anyhow!("load component actor {}: {error}", path.display()))?;
        let mut linker = Linker::<ActorStore>::new(&engine);
        bindings::BroodPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |store| store)
            .map_err(|error| anyhow::anyhow!("link Brood component actor imports: {error}"))?;

        let worker_filesystem = open_worker_filesystem(&config)?;
        // Start owned background work only after all fallible static loading
        // and capability setup has succeeded.
        let (completion_io, io_completion_rx, io_task) =
            start_completion_io(generation, &config, &io_runtime)?;
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let mailbox_slots = Arc::new(Semaphore::new(config.mailbox_backlog));
        let mailbox_capacity = config.mailbox_backlog;
        let observer_result_bytes = Arc::new(Mutex::new(HashMap::new()));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        // One dispatcher result can yield at most `command_capacity` host
        // records. Keep a second batch plus two reserved terminal slots so
        // DispatcherFailed/Stopped remain observable under saturation.
        let event_capacity = config.command_capacity.saturating_mul(2).saturating_add(2);
        let (event_tx, event_rx) = mpsc::channel(event_capacity);
        let event_observer = ComponentActorEventObserver::new(event_tx, config.event_waker.clone());
        let dispatcher_returned_pending = Arc::clone(&event_observer.dispatcher_returned_pending);
        let mut store = Store::new(
            &engine,
            ActorStore {
                completion_io: completion_io.clone(),
                coordination: coordination.clone(),
                filesystem_watches,
                runtime_io: runtime_io.clone(),
                actor_generation: generation,
                presentation: config.presentation.clone(),
                worker_filesystem,
                worker_cancellations: WorkerCancellationRegistry::default(),
                blocking_io: BlockingIoTracker::default(),
                event_observer: event_observer.clone(),
                shutdown_requested: Arc::clone(&shutdown_requested),
                limits: config.limits.store_limits(),
            },
        );
        store.limiter(|data| &mut data.limits);
        store.set_epoch_deadline(config.epoch_yield_ticks);
        store.epoch_deadline_async_yield_and_update(config.epoch_yield_ticks);
        let epoch_yield_ticks = config.epoch_yield_ticks;
        store.epoch_deadline_callback(move |store| {
            if store.data().shutdown_requested.load(Ordering::Acquire) {
                return Err(wasmtime::Error::msg("actor explicitly stopped"));
            }
            Ok(wasmtime::UpdateDeadline::Yield(epoch_yield_ticks))
        });
        let plugin = match epoch_ticker
            .drive(bindings::BroodPlugin::instantiate_async(
                &mut store, &component, &linker,
            ))
            .await
        {
            Ok(plugin) => plugin,
            Err(error) => {
                completion_io.stop();
                let _ = io_task.await;
                return Err(anyhow::anyhow!("instantiate component actor: {error}"));
            }
        };

        let (completion_tx, completion_rx) = mpsc::channel(config.max_outstanding_io);
        let runtime_completion_io = completion_io.clone();
        let runtime_byte_budget = kernel.byte_budget().clone();
        let runtime_observer_result_bytes = Arc::clone(&observer_result_bytes);
        let limits = config.limits;
        let runtime_epoch_ticker = epoch_ticker.clone();
        let runtime_task = tokio::spawn(async move {
            let result = run_component_actor(
                &mut store,
                plugin,
                command_rx,
                io_completion_rx,
                notification_rx,
                completion_tx,
                event_observer,
                config,
                runtime_byte_budget,
                runtime_observer_result_bytes,
                &runtime_epoch_ticker,
            )
            .await;
            runtime_completion_io.stop();
            result
        });

        Ok(Self {
            kernel,
            completion_io,
            commands: command_tx,
            mailbox_slots,
            mailbox_capacity,
            mailbox_enqueued: AtomicU64::new(0),
            mailbox_dropped: AtomicU64::new(0),
            events: event_rx,
            completions: completion_rx,
            next_worker: AtomicU64::new(1),
            runtime_task: Some(runtime_task),
            io_task: Some(io_task),
            epoch_ticker: Some(epoch_ticker),
            shutdown_requested,
            limits,
            observer_result_bytes,
            dispatcher_returned_pending,
        })
    }

    pub const fn generation(&self) -> u64 {
        self.kernel.generation()
    }

    /// Deliver one typed lifecycle hook and wait for explicit guest completion.
    pub async fn lifecycle(&self, event: PluginLifecycleEvent, timeout: Duration) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(RuntimeCommand::Lifecycle { event, reply })
            .await
            .context("send component lifecycle event")?;
        wait_for_lifecycle_reply(response, timeout)
            .await
            .context("wait for component lifecycle hook")?
            .map_err(anyhow::Error::msg)
    }

    pub const fn completion_io(&self) -> &CompletionIo {
        &self.completion_io
    }

    /// Snapshot live notification, pollable, and worker-wait registrations.
    pub fn coordination_stats(&self) -> crate::coordination::CoordinationStats {
        self.kernel.coordination_stats()
    }

    pub fn filesystem_watch_stats(&self) -> crate::filesystem_watch::FilesystemWatchStats {
        self.kernel.filesystem_watch_stats()
    }

    pub fn runtime_io_stats(&self) -> crate::runtime_io::RuntimeIoStats {
        self.kernel.runtime_io_stats()
    }

    /// Nonblocking actor-mailbox delivery.
    pub fn try_send_message(&self, sender: String, body: Vec<u8>) -> Result<()> {
        try_enqueue_mailbox(
            &self.commands,
            &self.mailbox_slots,
            &self.mailbox_enqueued,
            &self.mailbox_dropped,
            sender,
            body,
            self.limits,
            self.kernel.byte_budget(),
        )
    }

    /// Whether the actor runtime has stopped receiving mailbox commands.
    #[must_use]
    pub fn mailbox_is_closed(&self) -> bool {
        self.commands.is_closed()
    }

    /// Deliver one typed pane lifecycle or input event without blocking.
    pub fn try_send_presentation_event(
        &self,
        event: PresentationEvent,
    ) -> std::result::Result<(), ComponentActorSendError<PresentationEvent>> {
        let event_watermark = match &event {
            PresentationEvent::RenderHint(hint) => Some(hint.event_watermark),
            _ => None,
        };
        self.try_send_presentation_event_with_watermark(event, event_watermark)
    }

    /// Deliver a presentation event together with its host admission identity.
    pub fn try_send_presentation_event_with_watermark(
        &self,
        event: PresentationEvent,
        event_watermark: Option<u64>,
    ) -> std::result::Result<(), ComponentActorSendError<PresentationEvent>> {
        match self.try_send_presentation_events(vec![(event, event_watermark)]) {
            Ok(()) => Ok(()),
            Err(ComponentActorPresentationEnvelopeSendError::Full(mut events)) => {
                match events.pop() {
                    Some((event, _watermark)) => Err(ComponentActorSendError::Full(event)),
                    None => Err(ComponentActorSendError::InvariantViolation),
                }
            }
            Err(ComponentActorPresentationEnvelopeSendError::Closed(mut events)) => {
                match events.pop() {
                    Some((event, _watermark)) => Err(ComponentActorSendError::Closed(event)),
                    None => Err(ComponentActorSendError::InvariantViolation),
                }
            }
            Err(
                ComponentActorPresentationEnvelopeSendError::InvalidSize(_)
                | ComponentActorPresentationEnvelopeSendError::InvariantViolation,
            ) => Err(ComponentActorSendError::InvariantViolation),
        }
    }

    /// Deliver one bounded FIFO presentation envelope without blocking.
    pub fn try_send_presentation_events(
        &self,
        events: Vec<(PresentationEvent, Option<u64>)>,
    ) -> std::result::Result<(), ComponentActorPresentationEnvelopeSendError> {
        Self::try_send_presentation_envelope(&self.commands, events)
    }

    fn try_send_presentation_envelope(
        commands: &mpsc::Sender<RuntimeCommand>,
        events: Vec<(PresentationEvent, Option<u64>)>,
    ) -> std::result::Result<(), ComponentActorPresentationEnvelopeSendError> {
        if events.is_empty() || events.len() > MAX_PRESENTATION_BATCH_EVENTS {
            return Err(ComponentActorPresentationEnvelopeSendError::InvalidSize(
                events,
            ));
        }
        match commands.try_send(RuntimeCommand::Presentation { events }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(RuntimeCommand::Presentation { events })) => {
                Err(ComponentActorPresentationEnvelopeSendError::Full(events))
            }
            Err(mpsc::error::TrySendError::Closed(RuntimeCommand::Presentation { events })) => {
                Err(ComponentActorPresentationEnvelopeSendError::Closed(events))
            }
            Err(_) => Err(ComponentActorPresentationEnvelopeSendError::InvariantViolation),
        }
    }

    /// Deliver one typed session snapshot or command completion without
    /// blocking the presentation event loop.
    pub fn try_send_session_event(
        &self,
        event: SessionEvent,
    ) -> std::result::Result<(), ComponentActorSendError<SessionEvent>> {
        try_send_typed(&self.commands, event, RuntimeCommand::Session)
    }

    /// Deliver lifecycle feedback for one child owned by a blessed init actor.
    pub fn try_send_child_event(
        &self,
        event: ComponentChildEvent,
    ) -> std::result::Result<(), ComponentActorSendError<ComponentChildEvent>> {
        try_send_typed(&self.commands, event, RuntimeCommand::Child)
    }

    /// Deliver app lifecycle feedback to a blessed session init actor.
    pub fn try_send_app_event(
        &self,
        event: ComponentAppEvent,
    ) -> std::result::Result<(), ComponentActorSendError<ComponentAppEvent>> {
        try_send_typed(&self.commands, event, RuntimeCommand::App)
    }

    /// Snapshot cumulative mailbox delivery counters.
    pub fn mailbox_stats(&self) -> ComponentActorMailboxStats {
        ComponentActorMailboxStats {
            enqueued_messages: self.mailbox_enqueued.load(Ordering::Acquire),
            dropped_messages: self.mailbox_dropped.load(Ordering::Acquire),
            capacity: self.mailbox_capacity,
        }
    }

    /// Start an actor-private worker in the same component instance.
    pub fn try_spawn_worker(&self, payload: Vec<u8>) -> Result<WorkerId> {
        self.limits.ensure_worker_payload(&payload)?;
        let byte_permit = self.kernel.byte_budget().try_reserve(payload.len())?;
        let raw = self
            .next_worker
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| anyhow::anyhow!("worker id allocator is exhausted"))?;
        let worker = WorkerId(raw);
        self.commands
            .try_send(RuntimeCommand::SpawnWorker {
                worker,
                payload,
                byte_permit,
            })
            .map_err(|error| anyhow::anyhow!("component actor rejected worker spawn: {error}"))?;
        Ok(worker)
    }

    /// Request cooperative cancellation.
    ///
    /// The runtime wakes pending worker I/O and exposes the request at worker
    /// yield points. `WorkerCancelled` is emitted only after the guest export
    /// has returned, so it is a terminal lifecycle event rather than an
    /// observer-side suppression. A worker which never reaches a cooperative
    /// boundary is hard-stopped only with its owning actor Store.
    pub fn try_cancel_worker(&self, worker: WorkerId) -> Result<()> {
        self.commands
            .try_send(RuntimeCommand::CancelWorker(worker))
            .map_err(|error| {
                anyhow::anyhow!("component actor rejected worker cancellation: {error}")
            })
    }

    pub async fn recv_event(&mut self) -> Option<ComponentActorEvent> {
        let event = self.events.recv().await?;
        self.release_observer_result_bytes(&event);
        Some(event)
    }

    /// Drain one observer event without blocking the host actor.
    pub fn try_recv_event(&mut self) -> Option<ComponentActorEvent> {
        let event = self.events.try_recv().ok()?;
        self.release_observer_result_bytes(&event);
        Some(event)
    }

    /// Return the observer events already queued for the host without
    /// consuming them. Hosts use this after a bounded drain to distinguish a
    /// fully drained turn from one which must be resumed by a later callback.
    pub fn pending_event_count(&self) -> usize {
        self.events.len()
    }

    /// Observe the completion lane independently of normal mailbox traffic.
    pub async fn recv_completion(&mut self) -> Option<ActorCompletion> {
        self.completions.recv().await
    }

    fn release_observer_result_bytes(&self, event: &ComponentActorEvent) {
        if matches!(event, ComponentActorEvent::DispatcherReturned) {
            self.dispatcher_returned_pending
                .store(false, Ordering::Release);
        }
        let worker = match event {
            ComponentActorEvent::WorkerFinished { worker, .. }
            | ComponentActorEvent::WorkerFailed { worker, .. } => Some(*worker),
            _ => None,
        };
        if let Some(worker) = worker
            && let Ok(mut retained) = self.observer_result_bytes.lock()
        {
            retained.remove(&worker);
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.shutdown_requested.store(true, Ordering::Release);
        self.completion_io.stop();
        self.kernel.shutdown().await;
        let _ = self.commands.send(RuntimeCommand::Shutdown).await;
        let runtime_result = match self.runtime_task.take() {
            Some(task) => task
                .await
                .context("join component actor runtime")
                .and_then(|result| result),
            None => Ok(()),
        };
        let io_result = match self.io_task.take() {
            Some(task) => task.await.context("join component actor I/O supervisor"),
            None => Ok(()),
        };
        self.epoch_ticker.take();
        // Join every owned task even when the guest dispatcher trapped. This
        // keeps hot replacement structured while still reporting the guest
        // failure to the caller after cleanup is complete.
        runtime_result?;
        io_result?;
        Ok(())
    }
}

fn validate_presentation_backlog(capacity: usize) -> Result<()> {
    ensure!(
        capacity >= MAX_PRESENTATION_BATCH_EVENTS,
        "presentation backlog must reserve at least {MAX_PRESENTATION_BATCH_EVENTS} events"
    );
    Ok(())
}

fn increment_saturating(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_add(1))
    });
}

fn try_send_typed<T>(
    commands: &mpsc::Sender<RuntimeCommand>,
    event: T,
    wrap: impl FnOnce(T) -> RuntimeCommand,
) -> std::result::Result<(), ComponentActorSendError<T>> {
    match commands.try_reserve() {
        Ok(permit) => {
            permit.send(wrap(event));
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(())) => Err(ComponentActorSendError::Full(event)),
        Err(mpsc::error::TrySendError::Closed(())) => Err(ComponentActorSendError::Closed(event)),
    }
}

fn try_enqueue_mailbox(
    commands: &mpsc::Sender<RuntimeCommand>,
    mailbox_slots: &Arc<Semaphore>,
    mailbox_enqueued: &AtomicU64,
    mailbox_dropped: &AtomicU64,
    sender: String,
    body: Vec<u8>,
    limits: WasmResourceLimits,
    byte_budget: &GenerationByteBudget,
) -> Result<()> {
    if let Err(error) = limits.ensure_mailbox(&sender, &body) {
        increment_saturating(mailbox_dropped);
        return Err(error.context("component actor rejected mailbox message"));
    }
    let byte_permit = byte_budget.try_reserve(sender.len().saturating_add(body.len()))?;
    let Ok(permit) = Arc::clone(mailbox_slots).try_acquire_owned() else {
        increment_saturating(mailbox_dropped);
        return Err(anyhow::anyhow!("component actor mailbox backlog is full"));
    };
    let profile_id = if brood_profiler::enabled() {
        let id = brood_profiler::next_id();
        brood_profiler::flow(
            brood_profiler::Kind::FlowStart,
            "actor",
            "mailbox_enqueued",
            id,
            vec![
                ("sender".into(), sender.clone().into()),
                ("bytes".into(), body.len().into()),
            ],
        );
        id
    } else {
        0
    };
    match commands.try_send(RuntimeCommand::Mailbox {
        sender,
        body,
        permit,
        byte_permit,
        profile_id,
    }) {
        Ok(()) => {
            increment_saturating(mailbox_enqueued);
            Ok(())
        }
        Err(error) => {
            if profile_id != 0 {
                brood_profiler::flow(
                    brood_profiler::Kind::FlowEnd,
                    "actor",
                    "mailbox_rejected",
                    profile_id,
                    Vec::new(),
                );
            }
            increment_saturating(mailbox_dropped);
            Err(anyhow::anyhow!(
                "component actor mailbox rejected message: {error}"
            ))
        }
    }
}

impl Drop for ComponentActor {
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.completion_io.stop();
        self.kernel.stop();
        if let Some(task) = self.runtime_task.take() {
            task.abort();
        }
        if let Some(task) = self.io_task.take() {
            task.abort();
        }
        self.epoch_ticker.take();
    }
}

fn component_engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .concurrency_support(true)
        .epoch_interruption(true);
    Engine::new(&config)
        .map_err(|error| anyhow::anyhow!("create concurrent Component Model engine: {error}"))
}

fn open_worker_filesystem(config: &ComponentActorConfig) -> Result<Option<WorkerFilesystem>> {
    config
        .filesystem
        .as_ref()
        .map(|grant| {
            Ok(WorkerFilesystem {
                root: Arc::new(
                    Dir::open_ambient_dir(&grant.host_path, ambient_authority()).with_context(
                        || {
                            format!(
                                "open worker filesystem capability {}",
                                grant.host_path.display()
                            )
                        },
                    )?,
                ),
                writable: grant.writable,
                max_bytes: config.max_completion_bytes,
                max_replace_bytes: config.max_worker_replace_bytes,
            })
        })
        .transpose()
}

type GuestActor = bindings::exports::brood::plugin::actor::Guest;
type GuestEvent = bindings::exports::brood::plugin::actor::Event;
type GuestCommand = bindings::exports::brood::plugin::actor::Command;

enum GuestCallOutcome {
    Dispatch {
        result: wasmtime::Result<Vec<GuestCommand>>,
        context: DispatchContext,
    },
    Worker {
        worker: WorkerId,
        result: wasmtime::Result<std::result::Result<Vec<u8>, String>>,
    },
}

type GuestCallFuture<'a> = Pin<Box<dyn Future<Output = GuestCallOutcome> + Send + 'a>>;

struct QueuedGuestEvent {
    event: GuestEvent,
    _byte_permit: Option<GenerationBytePermit>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DispatchContext {
    event_watermark: Option<u64>,
    render_token: Option<RenderToken>,
    presentation_events: u8,
    profile_id: u64,
}

struct QueuedPresentationEvent {
    event: PresentationEvent,
    context: DispatchContext,
}

const MAX_PRESENTATION_BATCH_EVENTS: usize = 16;
const DEFAULT_PRESENTATION_BATCH_EVENTS: usize = 1;

fn normalize_presentation_batch_limit(requested: u8) -> usize {
    usize::from(requested).clamp(
        DEFAULT_PRESENTATION_BATCH_EVENTS,
        MAX_PRESENTATION_BATCH_EVENTS,
    )
}

async fn run_component_actor(
    store: &mut Store<ActorStore>,
    plugin: bindings::BroodPlugin,
    mut commands: mpsc::Receiver<RuntimeCommand>,
    mut io_completions: mpsc::Receiver<ActorCompletion>,
    mut notifications: mpsc::Receiver<NotificationId>,
    completion_observer: mpsc::Sender<ActorCompletion>,
    event_observer: ComponentActorEventObserver,
    config: ComponentActorConfig,
    byte_budget: GenerationByteBudget,
    observer_result_bytes: Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
    epoch_ticker: &EpochTicker,
) -> Result<()> {
    let guest = plugin.brood_plugin_actor();
    let stopped_observer = event_observer.clone();
    let blocking_io = store.data().blocking_io.clone();
    let concurrent_result = epoch_ticker
        .drive(store.run_concurrent(async move |accessor| {
            run_component_event_loop(
                accessor,
                guest,
                &mut commands,
                &mut io_completions,
                &mut notifications,
                &completion_observer,
                &event_observer,
                &config,
                &byte_budget,
                &observer_result_bytes,
            )
            .await
        }))
        .await;
    // Retire watch notifications and native subscriptions before the
    // generation can report termination or a replacement becomes reachable.
    store.data().filesystem_watches.stop();
    // Guest-task cancellation can drop an in-flight host future, but any
    // operation already delegated to the blocking pool remains owned by this
    // actor generation. Join those operations before reporting termination.
    blocking_io.wait_idle().await;
    let explicitly_stopped = store.data().shutdown_requested.load(Ordering::Acquire);
    let result = match concurrent_result {
        Ok(result) => component_loop_result(result, explicitly_stopped),
        Err(_error) if explicitly_stopped => Ok(()),
        Err(error) => {
            let message = format!("{error:#}");
            let _ = stopped_observer.try_send(ComponentActorEvent::DispatcherFailed {
                message: message.clone(),
            });
            return Err(anyhow::anyhow!(
                "run component actor concurrent Store: {message}"
            ));
        }
    };
    result?;
    let _ = stopped_observer.try_send(ComponentActorEvent::Stopped);
    Ok(())
}

fn component_loop_result(result: Result<()>, explicitly_stopped: bool) -> Result<()> {
    if explicitly_stopped && result.is_err() {
        return Ok(());
    }
    result
}

async fn run_component_event_loop(
    accessor: &wasmtime::component::Accessor<ActorStore>,
    guest: &GuestActor,
    commands: &mut mpsc::Receiver<RuntimeCommand>,
    io_completions: &mut mpsc::Receiver<ActorCompletion>,
    notifications: &mut mpsc::Receiver<NotificationId>,
    completion_observer: &mpsc::Sender<ActorCompletion>,
    event_observer: &ComponentActorEventObserver,
    config: &ComponentActorConfig,
    byte_budget: &GenerationByteBudget,
    observer_result_bytes: &Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
) -> Result<()> {
    use bindings::exports::brood::plugin::actor as abi;
    use futures::{FutureExt, StreamExt, stream::FuturesUnordered};

    let dispatch = guest.func_dispatch();
    let worker_run = guest.func_worker_run();
    let mut calls: FuturesUnordered<GuestCallFuture<'_>> = FuturesUnordered::new();
    let mut completion_queue = VecDeque::new();
    let mut notification_queue = VecDeque::new();
    let mut system_queue = VecDeque::new();
    let mut presentation_queue = VecDeque::new();
    let mut mailbox_queue = VecDeque::new();
    let mut dispatcher_busy = false;
    let mut active_workers = HashSet::new();
    let mut cancelled_workers = HashSet::new();
    let mut pending_lifecycle: Option<oneshot::Sender<std::result::Result<(), String>>> = None;
    let mut dispatch_lane = 0_usize;
    let mut presentation_batch_limit = DEFAULT_PRESENTATION_BATCH_EVENTS;

    loop {
        if component_dispatch_can_start(dispatcher_busy, event_observer) {
            let event = pop_component_dispatch(
                &mut dispatch_lane,
                &mut completion_queue,
                &mut notification_queue,
                &mut system_queue,
                &mut presentation_queue,
                &mut mailbox_queue,
                presentation_batch_limit,
                accessor,
                event_observer,
            )?;
            if let Some((event, mailbox_permit, byte_permit, context)) = event {
                dispatcher_busy = true;
                if context.profile_id != 0 {
                    brood_profiler::flow(
                        brood_profiler::Kind::FlowStep,
                        "actor",
                        "mailbox_dispatch",
                        context.profile_id,
                        Vec::new(),
                    );
                }
                let call = accessor
                    .with(|mut access| dispatch.start_call_concurrent(&mut access, (event,)))?;
                calls.push(
                    async move {
                        let _profile_span = brood_profiler::Span::asynchronous(
                            "wasm",
                            "actor_dispatch",
                            None,
                            Vec::new(),
                        );
                        // Admission remains held until the accepted mailbox
                        // message has reached its terminal dispatch result.
                        let _mailbox_permit = mailbox_permit;
                        let _byte_permit = byte_permit;
                        let result = dispatch
                            .finish_call_concurrent(accessor, call)
                            .await
                            .map(|(commands,)| commands);
                        GuestCallOutcome::Dispatch { result, context }
                    }
                    .boxed(),
                );
            }
        }

        tokio::select! {
            permits = event_observer.sender.reserve_many(ComponentActorEventObserver::TERMINAL_RESERVE + 1), if !event_observer.has_ordinary_capacity(1) => {
                // Waiting on a real channel permit makes observer saturation
                // pause this actor without polling or allocating a side queue.
                drop(permits);
            }
            permits = event_observer.sender.reserve_many(
                config.command_capacity
                    .saturating_add(ComponentActorEventObserver::TERMINAL_RESERVE + 1)
            ), if !calls.is_empty()
                && !event_observer.has_ordinary_capacity(config.command_capacity.saturating_add(1)) => {
                // A completed dispatch can return `command_capacity` observed
                // host records plus DispatcherReturned. Register a capacity
                // waiter so the actor is woken when the host drains output;
                // merely disabling `calls.next()` would sleep forever.
                drop(permits);
            }
            command = commands.recv(), if event_observer.has_ordinary_capacity(1)
                && presentation_command_lane_has_capacity(
                    presentation_queue.len(),
                    config.presentation_backlog,
                )
                && system_queue.len() < component_system_capacity(config)
                && mailbox_queue.len() < config.mailbox_backlog => match command {
                Some(RuntimeCommand::Mailbox { sender, body, permit, byte_permit, profile_id }) => {
                    // The sender acquired one of exactly `mailbox_backlog`
                    // permits before the command was accepted, so this queue
                    // cannot overflow or silently discard the message.
                    mailbox_queue.push_back((
                        abi::Event::Mailbox(abi::MailboxMessage { sender, body }),
                        permit,
                        byte_permit,
                        profile_id,
                    ));
                }
                Some(RuntimeCommand::SpawnWorker { worker, payload, byte_permit }) => {
                    let started = start_worker(
                        worker,
                        payload,
                        accessor,
                        worker_run,
                        &mut calls,
                        &mut system_queue,
                        &mut active_workers,
                        event_observer,
                        config,
                        byte_permit,
                    );
                    if started {
                        let _ = event_observer.try_send(ComponentActorEvent::WorkerStarted { worker });
                    }
                }
                Some(RuntimeCommand::CancelWorker(worker)) => {
                    request_component_worker_cancellation(
                        accessor,
                        worker,
                        &active_workers,
                        &mut cancelled_workers,
                    )?;
                }
                Some(RuntimeCommand::Lifecycle { event, reply }) => {
                    ensure!(pending_lifecycle.is_none(), "component lifecycle hook already pending");
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_lifecycle_to_guest(event),
                    );
                    pending_lifecycle = Some(reply);
                }
                Some(RuntimeCommand::Child(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_child_to_guest(event),
                    );
                }
                Some(RuntimeCommand::App(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_app_to_guest(event),
                    );
                }
                Some(RuntimeCommand::Presentation { events }) => {
                    for (event, event_watermark) in events {
                        enqueue_presentation_dispatch_with_watermark(
                            &mut presentation_queue,
                            config.presentation_backlog,
                            event,
                            event_watermark,
                        );
                    }
                }
                Some(RuntimeCommand::Session(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_session_to_guest(event),
                    );
                }
                Some(RuntimeCommand::Shutdown) | None => break,
            },
            completion = io_completions.recv(), if completion_queue.len() < config.max_outstanding_io
                && event_observer.has_ordinary_capacity(1) => {
                let Some(completion) = completion else { continue; };
                enqueue_dispatch(
                    &mut completion_queue,
                    config.max_outstanding_io,
                    completion_to_guest(&completion),
                );
                let _ = completion_observer.try_send(completion.clone());
                let _ = event_observer.try_send(ComponentActorEvent::CompletionDelivered(completion));
            }
            notification = notifications.recv(), if notification_queue.len() < config.max_notifications => {
                let Some(notification) = notification else { continue; };
                notification_queue.push_back(notification);
            }
            outcome = calls.next(), if !calls.is_empty()
                && event_observer.has_ordinary_capacity(config.command_capacity.saturating_add(1))
                && system_queue.len().saturating_add(config.command_capacity)
                    <= component_system_capacity(config) => {
                let Some(outcome) = outcome else { continue; };
                match outcome {
                    GuestCallOutcome::Dispatch { result, context } => {
                        dispatcher_busy = false;
                        if context.profile_id != 0 {
                            brood_profiler::flow(
                                brood_profiler::Kind::FlowEnd,
                                "actor",
                                "mailbox_returned",
                                context.profile_id,
                                vec![("success".into(), result.is_ok().into())],
                            );
                        }
                        match result {
                            Ok(returned) => {
                                ensure!(
                                    returned.len() <= config.command_capacity,
                                    "actor dispatcher returned too many commands"
                                );
                                let completed = if context.event_watermark.is_some()
                                    || context.render_token.is_some()
                                {
                                    ComponentActorEvent::PresentationTurnReturned {
                                        event_watermark: context.event_watermark,
                                        render_token: context.render_token,
                                        presentation_events: context.presentation_events,
                                    }
                                } else {
                                    ComponentActorEvent::DispatcherReturned
                                };
                                let _ = event_observer.try_send(completed);
                                for command in returned {
                                    match command {
                                        abi::Command::SpawnWorker(request) => {
                                            let worker = WorkerId(request.worker);
                                            let byte_permit = match byte_budget.try_reserve(request.payload.len()) {
                                                Ok(permit) => permit,
                                                Err(error) => {
                                                    enqueue_dispatch(
                                                        &mut system_queue,
                                                        component_system_capacity(config),
                                                        abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                                                            worker: worker.raw(),
                                                            message: error.to_string(),
                                                        })),
                                                    );
                                                    continue;
                                                }
                                            };
                                            let started = start_worker(
                                                worker,
                                                request.payload,
                                                accessor,
                                                worker_run,
                                                &mut calls,
                                                &mut system_queue,
                                                &mut active_workers,
                                                event_observer,
                                                config,
                                                byte_permit,
                                            );
                                            if started {
                                                let _ = event_observer.try_send(ComponentActorEvent::WorkerStarted { worker });
                                            }
                                        }
                                        abi::Command::CancelWorker(worker) => {
                                            let worker = WorkerId(worker);
                                            request_component_worker_cancellation(
                                                accessor,
                                                worker,
                                                &active_workers,
                                                &mut cancelled_workers,
                                            )?;
                                        }
                                        abi::Command::LifecycleComplete(result) => {
                                            let reply = pending_lifecycle.take().context(
                                                "component guest completed lifecycle with no pending hook",
                                            )?;
                                            let _ = reply.send(result);
                                        }
                                        abi::Command::StartActor(request) => {
                                            ensure!(
                                                config.actor_management,
                                                "actor start requested without realm init capability"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::ActorStartRequested {
                                                    id: request.id,
                                                    path: request.path,
                                                    capabilities: request.capabilities,
                                                    presentation_scope: match request.presentation_scope {
                                                        abi::PresentationScope::Global => PresentationScope::Global,
                                                        abi::PresentationScope::Session(session) => PresentationScope::Session {
                                                            backend_id: session.backend_id,
                                                            session_id: session.session_id,
                                                        },
                                                    },
                                                })
                                                .context("realm init actor request queue is full")?;
                                        }
                                        abi::Command::StopActor(id) => {
                                            ensure!(
                                                config.actor_management,
                                                "actor stop requested without realm init capability"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::ActorStopRequested { id })
                                                .context("realm init actor request queue is full")?;
                                        }
                                        abi::Command::StartApp(request) => {
                                            ensure!(
                                                config.app_management,
                                                "app start requested without session init capability"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::AppStartRequested {
                                                    id: request.id,
                                                    argv: request.argv,
                                                })
                                                .context("realm init actor request queue is full")?;
                                        }
                                        abi::Command::SendSessionMessage(request) => {
                                            ensure!(
                                                config.session_messaging,
                                                "session message requested outside a presentation realm"
                                            );
                                            ensure!(
                                                request.session_id != 0 && !request.actor.trim().is_empty(),
                                                "session actor address is invalid"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::SessionMessageRequested {
                                                    backend_id: request.backend_id,
                                                    session_id: request.session_id,
                                                    actor: request.actor,
                                                    body: request.body,
                                                })
                                                .context("session actor message request queue is full")?;
                                        }
                                        abi::Command::SendDaemonMessage(request) => {
                                            ensure!(
                                                config.daemon_messaging,
                                                "daemon message requested outside a presentation realm"
                                            );
                                            ensure!(
                                                !request.actor.trim().is_empty(),
                                                "daemon actor address is invalid"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::DaemonMessageRequested {
                                                    backend_id: request.backend_id,
                                                    actor: request.actor,
                                                    body: request.body,
                                                })
                                                .context("daemon actor message request queue is full")?;
                                        }
                                        abi::Command::SendLocalMessage(request) => {
                                            ensure!(
                                                !request.actor.trim().is_empty(),
                                                "local actor address is invalid"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::LocalMessageRequested {
                                                    actor: request.actor,
                                                    body: request.body,
                                                })
                                                .context("local actor message request queue is full")?;
                                        }
                                        abi::Command::SendPresentationMessage(request) => {
                                            ensure!(
                                                config.presentation_messaging,
                                                "presentation message requested outside a session realm"
                                            );
                                            ensure!(
                                                !request.actor.trim().is_empty(),
                                                "presentation actor address is invalid"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::PresentationMessageRequested {
                                                    actor: request.actor,
                                                    body: request.body,
                                                })
                                                .context("presentation actor message request queue is full")?;
                                        }
                                        abi::Command::SessionCommand(request) => {
                                            ensure!(
                                                config.session_control,
                                                "session command requested outside a client realm"
                                            );
                                            let request = component_session_command_from_guest(request)?;
                                            event_observer
                                                .try_send(ComponentActorEvent::SessionCommandRequested(request))
                                                .context("session command request queue is full")?;
                                        }
                                        abi::Command::SendHostMessage(payload) => {
                                            ensure!(
                                                payload.len() <= crate::MAX_MAILBOX_MESSAGE_BYTES,
                                                "component host message exceeds the mailbox message byte limit"
                                            );
                                            event_observer
                                                .try_send(ComponentActorEvent::HostMessageRequested {
                                                    payload,
                                                })
                                                .context("component host message request queue is full")?;
                                        }
                                        abi::Command::SetPresentationBatchLimit(limit) => {
                                            presentation_batch_limit = normalize_presentation_batch_limit(limit);
                                        }
                                        abi::Command::Stop => return Ok(()),
                                    }
                                }
                            }
                            Err(error) => {
                                let explicitly_stopped = accessor.with(|mut access| {
                                    Ok::<_, wasmtime::Error>(
                                        access
                                            .get()
                                            .shutdown_requested
                                            .load(Ordering::Acquire),
                                    )
                                })?;
                                if explicitly_stopped {
                                    return Ok(());
                                }
                                let message = error.to_string();
                                let _ = event_observer.try_send(ComponentActorEvent::DispatcherFailed { message: message.clone() });
                                anyhow::bail!("component actor dispatcher failed: {message}");
                            }
                        }
                    }
                    GuestCallOutcome::Worker { worker, result } => {
                        active_workers.remove(&worker);
                        accessor.with(|mut access| {
                            access.get().worker_cancellations.remove(worker)
                        })?;
                        if cancelled_workers.remove(&worker) {
                            enqueue_dispatch(
                                &mut system_queue,
                                component_system_capacity(config),
                                abi::Event::Worker(abi::WorkerEvent::Cancelled(worker.raw())),
                            );
                            let _ = event_observer.try_send(ComponentActorEvent::WorkerCancelled { worker });
                            continue;
                        }
                        match result {
                            Ok(Ok(output)) => {
                                if let Err(error) = config.limits.ensure_worker_result(&output) {
                                    let message = error.to_string();
                                    enqueue_dispatch(
                                        &mut system_queue,
                                        component_system_capacity(config),
                                        abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                                            worker: worker.raw(),
                                            message: message.clone(),
                                        })),
                                    );
                                    let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
                                    continue;
                                }
                                let result_permits = match (
                                    byte_budget.try_reserve(output.len()),
                                    byte_budget.try_reserve(output.len()),
                                ) {
                                    (Ok(system), Ok(observer)) => (system, observer),
                                    (Err(error), _) | (_, Err(error)) => {
                                        let message = error.to_string();
                                        enqueue_dispatch(
                                            &mut system_queue,
                                            component_system_capacity(config),
                                            abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                                                worker: worker.raw(),
                                                message: message.clone(),
                                            })),
                                        );
                                        let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
                                        continue;
                                    }
                                };
                                enqueue_retained_dispatch(
                                    &mut system_queue,
                                    component_system_capacity(config),
                                    abi::Event::Worker(abi::WorkerEvent::Finished(abi::WorkerFinished {
                                        worker: worker.raw(),
                                        output: output.clone(),
                                    })),
                                    result_permits.0,
                                );
                                observe_retained_worker_event(
                                    event_observer,
                                    observer_result_bytes,
                                    worker,
                                    ComponentActorEvent::WorkerFinished { worker, output },
                                    result_permits.1,
                                )?;
                            }
                            Ok(Err(message)) => {
                                let message = match config.limits.ensure_worker_result(message.as_bytes()) {
                                    Ok(()) => message,
                                    Err(error) => error.to_string(),
                                };
                                let result_permits = byte_budget
                                    .try_reserve(message.len())
                                    .and_then(|system| {
                                        byte_budget
                                            .try_reserve(message.len())
                                            .map(|observer| (system, observer))
                                    })
                                    .ok();
                                let message = if result_permits.is_some() {
                                    message
                                } else {
                                    "generation host byte budget rejected worker error".to_string()
                                };
                                let event = abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                                    worker: worker.raw(),
                                    message: message.clone(),
                                }));
                                if let Some((system_permit, observer_permit)) = result_permits {
                                    enqueue_retained_dispatch(
                                        &mut system_queue,
                                        component_system_capacity(config),
                                        event,
                                        system_permit,
                                    );
                                    observe_retained_worker_event(
                                        event_observer,
                                        observer_result_bytes,
                                        worker,
                                        ComponentActorEvent::WorkerFailed { worker, message },
                                        observer_permit,
                                    )?;
                                } else {
                                    enqueue_dispatch(
                                        &mut system_queue,
                                        component_system_capacity(config),
                                        event,
                                    );
                                    let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
                                }
                            }
                            Err(error) => {
                                let message = error.to_string();
                                enqueue_dispatch(
                                    &mut system_queue,
                                    component_system_capacity(config),
                                    abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                                        worker: worker.raw(),
                                        message: message.clone(),
                                    })),
                                );
                                let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn component_lifecycle_to_guest(event: PluginLifecycleEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;
    let event = match event {
        PluginLifecycleEvent::Start(context) => {
            let reason = match context.reason {
                PluginStartReason::Initial => abi::StartReason::Initial,
                PluginStartReason::Replacement => abi::StartReason::Replacement,
                PluginStartReason::SessionRebind => abi::StartReason::SessionRebind,
            };
            abi::LifecycleEvent::Start(abi::StartContext {
                instance_id: context.instance_id,
                generation: context.generation,
                reason,
                backend_id: context.backend_id,
                session_id: context.session_id,
                workspace_root: context.workspace_root,
                data: context
                    .data
                    .into_iter()
                    .map(|entry| abi::LifecycleEntry {
                        key: entry.key,
                        value: entry.value,
                    })
                    .collect(),
            })
        }
        PluginLifecycleEvent::Stop(context) => {
            let reason = match context.reason {
                PluginStopReason::Replacement => abi::StopReason::Replacement,
                PluginStopReason::SessionRebind => abi::StopReason::SessionRebind,
                PluginStopReason::Shutdown => abi::StopReason::Shutdown,
            };
            abi::LifecycleEvent::Stop(abi::StopContext {
                instance_id: context.instance_id,
                generation: context.generation,
                reason,
                next_generation: context.next_generation,
            })
        }
    };
    abi::Event::Lifecycle(event)
}

fn component_presentation_to_guest(event: PresentationEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;
    abi::Event::Presentation(component_presentation_to_guest_event(event))
}

fn component_presentation_to_guest_event(
    event: PresentationEvent,
) -> bindings::exports::brood::plugin::actor::PresentationEvent {
    use bindings::exports::brood::plugin::actor as abi;
    match event {
        PresentationEvent::PaneCreated {
            pane,
            width,
            height,
            scale,
        } => abi::PresentationEvent::PaneCreated(abi::PaneCreated {
            pane,
            width,
            height,
            scale,
        }),
        PresentationEvent::PaneResized {
            pane,
            width,
            height,
            scale,
            resize_epoch,
        } => abi::PresentationEvent::PaneResized(abi::PaneResized {
            pane,
            width,
            height,
            scale,
            resize_epoch,
        }),
        PresentationEvent::PaneFocused { pane, focused } => {
            abi::PresentationEvent::PaneFocused(abi::PaneFocused { pane, focused })
        }
        PresentationEvent::PointerMoved { pane, x, y } => {
            abi::PresentationEvent::PointerMoved(abi::PointerMoved { pane, x, y })
        }
        PresentationEvent::PointerLeft { pane } => {
            abi::PresentationEvent::PointerLeft(abi::PointerLeft { pane })
        }
        PresentationEvent::PointerButton {
            pane,
            x,
            y,
            button,
            pressed,
            modifiers,
        } => abi::PresentationEvent::PointerButton(abi::PointerButton {
            pane,
            x,
            y,
            button,
            pressed,
            modifiers,
        }),
        PresentationEvent::PointerScroll {
            pane,
            x,
            y,
            horizontal,
            vertical,
            modifiers,
        } => abi::PresentationEvent::PointerScroll(abi::PointerScroll {
            pane,
            x,
            y,
            horizontal,
            vertical,
            modifiers,
        }),
        PresentationEvent::Key {
            pane,
            physical_key,
            logical_key,
            pressed,
            repeat,
            modifiers,
        } => abi::PresentationEvent::Key(abi::KeyEvent {
            pane,
            physical_key,
            logical_key,
            pressed,
            repeat,
            modifiers,
        }),
        PresentationEvent::Text { pane, text } => {
            abi::PresentationEvent::Text(abi::TextEvent { pane, text })
        }
        PresentationEvent::Paste { pane, text } => {
            abi::PresentationEvent::Paste(abi::PasteEvent { pane, text })
        }
        PresentationEvent::Image {
            pane,
            name,
            mime_type,
            bytes,
            preview_width,
            preview_height,
            preview_rgba8,
        } => abi::PresentationEvent::Image(abi::ImageEvent {
            pane,
            name,
            mime_type,
            bytes,
            preview_width,
            preview_height,
            preview_rgba8,
        }),
        PresentationEvent::RenderHint(RenderHint {
            token,
            target_presentation_time_ns,
            submission_deadline_ns,
            relevant_panes,
            event_watermark,
        }) => abi::PresentationEvent::RenderHint(abi::RenderHint {
            token: abi::RenderToken { opaque: token.0 },
            target_presentation_time_ns,
            submission_deadline_ns,
            relevant_panes,
            event_watermark,
        }),
    }
}

fn component_session_to_guest(event: SessionEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;

    let event = match event {
        SessionEvent::Snapshot(snapshot) => abi::SessionEvent::Snapshot(abi::SessionSnapshot {
            backend_id: snapshot.backend_id,
            revision: snapshot.revision,
            active: snapshot.active,
            sessions: snapshot
                .sessions
                .into_iter()
                .map(component_session_summary_to_guest)
                .collect(),
        }),
        SessionEvent::CommandCompleted(completion) => {
            abi::SessionEvent::CommandCompleted(abi::SessionCommandCompleted {
                backend_id: completion.backend_id,
                request: completion.request,
                operation: component_session_operation_to_guest(completion.operation),
                success: completion.success,
                session: completion.session,
                message: completion.message,
            })
        }
    };
    abi::Event::Session(event)
}

fn component_session_summary_to_guest(
    session: SessionSummary,
) -> bindings::exports::brood::plugin::actor::SessionSummary {
    use bindings::exports::brood::plugin::actor as abi;
    use brood_protocol::{
        CapsuleFailureStage, CapsuleLifecycle, IsolationProfile, SessionVolumeLifecycle,
    };

    abi::SessionSummary {
        id: session.id,
        name: session.name,
        active: session.active,
        windows: session.windows,
        working_directory: session.working_directory,
        project_id: session.project_id,
        project_name: session.project_name,
        project_directory: session.project_directory,
        branch: session.branch,
        worktree: session.worktree,
        git: session.git,
        isolation: abi::SessionIsolationStatus {
            profile: match session.isolation.profile {
                IsolationProfile::Isolated => abi::IsolationProfile::Isolated,
                IsolationProfile::Direct => abi::IsolationProfile::Direct,
            },
            volume: session
                .isolation
                .volume
                .map(|volume| abi::SessionVolumeStatus {
                    id: volume.id.to_string(),
                    lifecycle: match volume.lifecycle {
                        SessionVolumeLifecycle::Active => abi::SessionVolumeLifecycle::Active,
                        SessionVolumeLifecycle::Retained => abi::SessionVolumeLifecycle::Retained,
                        SessionVolumeLifecycle::Graveyard => abi::SessionVolumeLifecycle::Graveyard,
                        SessionVolumeLifecycle::Purged => abi::SessionVolumeLifecycle::Purged,
                    },
                }),
            capsule: session.isolation.capsule.map(|capsule| abi::CapsuleStatus {
                lifecycle: match capsule.lifecycle {
                    CapsuleLifecycle::Preparing => abi::CapsuleLifecycle::Preparing,
                    CapsuleLifecycle::Ready => abi::CapsuleLifecycle::Ready,
                    CapsuleLifecycle::Stopping => abi::CapsuleLifecycle::Stopping,
                    CapsuleLifecycle::Stopped => abi::CapsuleLifecycle::Stopped,
                    CapsuleLifecycle::Failed => abi::CapsuleLifecycle::Failed,
                },
                failure: capsule.failure.map(|failure| abi::CapsuleFailure {
                    stage: match failure.stage {
                        CapsuleFailureStage::Preparation => abi::CapsuleFailureStage::Preparation,
                        CapsuleFailureStage::Runtime => abi::CapsuleFailureStage::Runtime,
                        CapsuleFailureStage::Cleanup => abi::CapsuleFailureStage::Cleanup,
                    },
                    message: failure.message,
                }),
            }),
        },
    }
}

fn component_session_command_from_guest(
    request: bindings::exports::brood::plugin::actor::SessionCommand,
) -> Result<SessionCommandRequest> {
    use bindings::exports::brood::plugin::actor as abi;

    let operation = match request.operation {
        abi::SessionOperation::Refresh => SessionOperation::Refresh,
        abi::SessionOperation::Activate => SessionOperation::Activate,
        abi::SessionOperation::Create => SessionOperation::Create,
        abi::SessionOperation::Rename => SessionOperation::Rename,
        abi::SessionOperation::Close => SessionOperation::Close,
    };
    match operation {
        SessionOperation::Refresh => ensure!(
            request.session.is_none() && request.name.is_none(),
            "refresh session command takes no arguments"
        ),
        SessionOperation::Activate | SessionOperation::Close => ensure!(
            request.session.is_some_and(|session| session != 0) && request.name.is_none(),
            "session command requires one non-zero session ID"
        ),
        SessionOperation::Create => ensure!(
            request.session.is_none(),
            "create session command does not accept a session ID"
        ),
        SessionOperation::Rename => ensure!(
            request.session.is_some_and(|session| session != 0)
                && request
                    .name
                    .as_ref()
                    .is_some_and(|name| !name.trim().is_empty()),
            "rename session command requires a session ID and non-empty name"
        ),
    }
    Ok(SessionCommandRequest {
        backend_id: request.backend_id,
        request: request.request,
        operation,
        session: request.session,
        name: request.name,
        directory: request.directory,
    })
}

const fn component_session_operation_to_guest(
    operation: SessionOperation,
) -> bindings::exports::brood::plugin::actor::SessionOperation {
    use bindings::exports::brood::plugin::actor as abi;
    match operation {
        SessionOperation::Refresh => abi::SessionOperation::Refresh,
        SessionOperation::Activate => abi::SessionOperation::Activate,
        SessionOperation::Create => abi::SessionOperation::Create,
        SessionOperation::Rename => abi::SessionOperation::Rename,
        SessionOperation::Close => abi::SessionOperation::Close,
    }
}

fn component_child_to_guest(event: ComponentChildEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;

    let event = match event {
        ComponentChildEvent::Started { id, generation } => {
            abi::ChildEvent::Started(abi::ActorStarted { id, generation })
        }
        ComponentChildEvent::Stopped { id } => abi::ChildEvent::Stopped(id),
        ComponentChildEvent::Failed { id, message } => {
            abi::ChildEvent::Failed(abi::ActorFailed { id, message })
        }
    };
    abi::Event::Child(event)
}

fn component_app_to_guest(event: ComponentAppEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;

    let event = match event {
        ComponentAppEvent::Started { id, pane } => {
            abi::AppEvent::Started(abi::AppStarted { id, pane })
        }
        ComponentAppEvent::Stopped { id } => abi::AppEvent::Stopped(id),
        ComponentAppEvent::Failed { id, message } => {
            abi::AppEvent::Failed(abi::AppFailed { id, message })
        }
    };
    abi::Event::App(event)
}

fn pop_component_dispatch(
    next_lane: &mut usize,
    completion_queue: &mut VecDeque<QueuedGuestEvent>,
    notification_queue: &mut VecDeque<NotificationId>,
    system_queue: &mut VecDeque<QueuedGuestEvent>,
    presentation_queue: &mut VecDeque<QueuedPresentationEvent>,
    mailbox_queue: &mut VecDeque<(GuestEvent, OwnedSemaphorePermit, GenerationBytePermit, u64)>,
    presentation_batch_limit: usize,
    accessor: &wasmtime::component::Accessor<ActorStore>,
    event_observer: &ComponentActorEventObserver,
) -> Result<
    Option<(
        GuestEvent,
        Option<OwnedSemaphorePermit>,
        Option<GenerationBytePermit>,
        DispatchContext,
    )>,
> {
    use bindings::exports::brood::plugin::actor as abi;

    for _lane in 0..5 {
        let lane = *next_lane;
        *next_lane = (*next_lane + 1) % 5;
        let event = match lane {
            0 => completion_queue
                .pop_front()
                .map(|queued| (queued.event, None, None, DispatchContext::default())),
            1 => {
                let mut selected = None;
                while let Some(notification) = notification_queue.pop_front() {
                    let event = accessor.with(|mut access| {
                        access.get().coordination.take_actor_event(notification)
                    })?;
                    let Some(event) = event else {
                        continue;
                    };
                    let _ = event_observer.try_send(ComponentActorEvent::NotificationDelivered {
                        notification: event.notification,
                        generation: event.generation,
                        token: event.token,
                    });
                    selected = Some((
                        abi::Event::Notification(abi::NotificationEvent {
                            notification: event.notification.raw(),
                            generation: event.generation,
                            token: event.token,
                        }),
                        None,
                        None,
                        DispatchContext::default(),
                    ));
                    break;
                }
                selected
            }
            2 => system_queue
                .pop_front()
                .map(|queued| (queued.event, None, None, DispatchContext::default())),
            3 => pop_presentation_dispatch(presentation_queue, presentation_batch_limit)?
                .map(|(event, context)| (event, None, None, context)),
            _ => mailbox_queue
                .pop_front()
                .map(|(event, permit, byte_permit, profile_id)| {
                    (
                        event,
                        Some(permit),
                        Some(byte_permit),
                        DispatchContext {
                            profile_id,
                            ..DispatchContext::default()
                        },
                    )
                }),
        };
        if event.is_some() {
            return Ok(event);
        }
    }
    Ok(None)
}

fn pop_presentation_dispatch(
    queue: &mut VecDeque<QueuedPresentationEvent>,
    batch_limit: usize,
) -> Result<Option<(GuestEvent, DispatchContext)>> {
    use bindings::exports::brood::plugin::actor as abi;

    let Some(first) = queue.pop_front() else {
        return Ok(None);
    };
    if batch_limit <= 1 || presentation_event_is_batch_boundary(&first.event) {
        return Ok(Some((
            component_presentation_to_guest(first.event),
            first.context,
        )));
    }
    let mut events = vec![first.event];
    let mut context = first.context;
    while events.len() < batch_limit.min(MAX_PRESENTATION_BATCH_EVENTS) {
        if queue
            .front()
            .is_none_or(|queued| presentation_event_is_batch_boundary(&queued.event))
        {
            break;
        }
        let Some(queued) = queue.pop_front() else {
            anyhow::bail!("presentation queue changed between inspection and removal");
        };
        context.event_watermark = match (context.event_watermark, queued.context.event_watermark) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
        events.push(queued.event);
    }
    context.presentation_events = u8::try_from(events.len())
        .map_err(|_| anyhow::anyhow!("presentation batch length does not fit the guest ABI"))?;
    let event = if events.len() == 1 {
        let Some(event) = events.pop() else {
            anyhow::bail!("singleton presentation batch lost its event");
        };
        component_presentation_to_guest(event)
    } else {
        abi::Event::PresentationBatch(
            events
                .into_iter()
                .map(component_presentation_to_guest_event)
                .collect(),
        )
    };
    Ok(Some((event, context)))
}

const fn presentation_event_is_batch_boundary(event: &PresentationEvent) -> bool {
    matches!(
        event,
        PresentationEvent::RenderHint(_)
            | PresentationEvent::Paste { .. }
            | PresentationEvent::Image { .. }
    )
}

const fn presentation_command_lane_has_capacity(queue_len: usize, capacity: usize) -> bool {
    // RuntimeCommand is a shared lane, so its next variant cannot be inspected
    // before receive. Reserve the largest legal presentation envelope. The
    // sender retains an unreceived envelope in its bounded command channel,
    // preserving FIFO without letting a 16-event envelope expand past the
    // reliable presentation queue's hard cap.
    queue_len.saturating_add(MAX_PRESENTATION_BATCH_EVENTS) <= capacity
}

#[cfg(test)]
fn enqueue_presentation_dispatch(
    queue: &mut VecDeque<QueuedPresentationEvent>,
    capacity: usize,
    event: PresentationEvent,
) {
    enqueue_presentation_dispatch_with_watermark(queue, capacity, event, None);
}

fn enqueue_presentation_dispatch_with_watermark(
    queue: &mut VecDeque<QueuedPresentationEvent>,
    capacity: usize,
    event: PresentationEvent,
    event_watermark: Option<u64>,
) {
    let render_token = match &event {
        PresentationEvent::RenderHint(hint) => Some(hint.token),
        _ => None,
    };
    let queued_event = QueuedPresentationEvent {
        event,
        context: DispatchContext {
            event_watermark,
            render_token,
            presentation_events: 1,
            ..DispatchContext::default()
        },
    };
    match &queued_event.event {
        PresentationEvent::PointerMoved { pane, .. } => {
            if let Some(queued) = queue.back_mut()
                && matches!(&queued.event, PresentationEvent::PointerMoved { pane: queued_pane, .. } if *queued_pane == *pane)
            {
                queued.context.event_watermark = match (
                    queued.context.event_watermark,
                    queued_event.context.event_watermark,
                ) {
                    (Some(left), Some(right)) => Some(left.max(right)),
                    (left, right) => left.or(right),
                };
                let context = queued.context;
                *queued = queued_event;
                queued.context = context;
                return;
            }
            if queue.len() < capacity {
                queue.push_back(queued_event);
            }
            return;
        }
        PresentationEvent::PaneResized { pane, .. } => {
            if let Some(queued) = queue.back_mut()
                && matches!(&queued.event, PresentationEvent::PaneResized { pane: queued_pane, .. } if *queued_pane == *pane)
            {
                queued.context.event_watermark = match (
                    queued.context.event_watermark,
                    queued_event.context.event_watermark,
                ) {
                    (Some(left), Some(right)) => Some(left.max(right)),
                    (left, right) => left.or(right),
                };
                let context = queued.context;
                *queued = queued_event;
                queued.context = context;
                return;
            }
        }
        PresentationEvent::PaneCreated { .. }
        | PresentationEvent::PaneFocused { .. }
        | PresentationEvent::PointerLeft { .. }
        | PresentationEvent::PointerButton { .. }
        | PresentationEvent::PointerScroll { .. }
        | PresentationEvent::Key { .. }
        | PresentationEvent::Text { .. }
        | PresentationEvent::Paste { .. }
        | PresentationEvent::Image { .. }
        | PresentationEvent::RenderHint(_) => {}
    }

    if queue.len() >= capacity
        && let Some(index) = queue
            .iter()
            .position(|queued| matches!(queued.event, PresentationEvent::PointerMoved { .. }))
    {
        queue.remove(index);
    }
    assert!(
        queue.len() < capacity,
        "reliable presentation event exceeded reserved queue capacity"
    );
    queue.push_back(queued_event);
}

fn enqueue_dispatch(
    queue: &mut VecDeque<QueuedGuestEvent>,
    _soft_capacity: usize,
    event: GuestEvent,
) {
    queue.push_back(QueuedGuestEvent {
        event,
        _byte_permit: None,
    });
}

fn enqueue_retained_dispatch(
    queue: &mut VecDeque<QueuedGuestEvent>,
    _soft_capacity: usize,
    event: GuestEvent,
    byte_permit: GenerationBytePermit,
) {
    queue.push_back(QueuedGuestEvent {
        event,
        _byte_permit: Some(byte_permit),
    });
}

fn observe_retained_worker_event(
    observer: &ComponentActorEventObserver,
    retained: &Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
    worker: WorkerId,
    event: ComponentActorEvent,
    byte_permit: GenerationBytePermit,
) -> Result<()> {
    {
        let mut retained = retained
            .lock()
            .map_err(|error| anyhow::anyhow!("worker result byte budget is poisoned: {error}"))?;
        ensure!(
            retained.insert(worker, byte_permit).is_none(),
            "worker {} already has a retained observer result",
            worker.raw()
        );
        drop(retained);
    }
    if observer.try_send(event).is_err() {
        let mut retained = retained
            .lock()
            .map_err(|error| anyhow::anyhow!("worker result byte budget is poisoned: {error}"))?;
        retained.remove(&worker);
        drop(retained);
    }
    Ok(())
}

fn start_worker<'a>(
    worker: WorkerId,
    payload: Vec<u8>,
    accessor: &'a wasmtime::component::Accessor<ActorStore>,
    worker_run: wasmtime::component::TypedFunc<
        (u64, Vec<u8>),
        (std::result::Result<Vec<u8>, String>,),
    >,
    calls: &mut futures::stream::FuturesUnordered<GuestCallFuture<'a>>,
    system_queue: &mut VecDeque<QueuedGuestEvent>,
    active_workers: &mut HashSet<WorkerId>,
    event_observer: &ComponentActorEventObserver,
    config: &ComponentActorConfig,
    byte_permit: GenerationBytePermit,
) -> bool {
    use bindings::exports::brood::plugin::actor as abi;
    use futures::FutureExt;

    let admission_failure = if let Err(error) = config.limits.ensure_worker_payload(&payload) {
        Some(error.to_string())
    } else if active_workers.contains(&worker) {
        Some(format!(
            "component actor worker {} is already active",
            worker.raw()
        ))
    } else if active_workers.len() >= config.max_workers {
        Some("component actor worker limit reached".to_string())
    } else {
        None
    };
    if let Some(message) = admission_failure {
        enqueue_dispatch(
            system_queue,
            component_system_capacity(config),
            abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                worker: worker.raw(),
                message: message.clone(),
            })),
        );
        let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
        return false;
    }

    active_workers.insert(worker);
    let call = match accessor.with(|mut access| {
        let call = worker_run.start_call_concurrent(&mut access, (worker.raw(), payload))?;
        access
            .get()
            .worker_cancellations
            .register(worker, call.task())
            .map_err(|error| wasmtime::Error::msg(error.to_string()))?;
        Ok::<_, wasmtime::Error>(call)
    }) {
        Ok(call) => call,
        Err(error) => {
            active_workers.remove(&worker);
            let message = format!("component worker could not start: {error}");
            enqueue_dispatch(
                system_queue,
                component_system_capacity(config),
                abi::Event::Worker(abi::WorkerEvent::Failed(abi::WorkerFailed {
                    worker: worker.raw(),
                    message: message.clone(),
                })),
            );
            let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
            return false;
        }
    };
    enqueue_dispatch(
        system_queue,
        component_system_capacity(config),
        abi::Event::Worker(abi::WorkerEvent::Started(worker.raw())),
    );
    calls.push(
        async move {
            let _byte_permit = byte_permit;
            let result = worker_run
                .finish_call_concurrent(accessor, call)
                .await
                .map(|(result,)| result);
            GuestCallOutcome::Worker { worker, result }
        }
        .boxed(),
    );
    true
}

fn request_component_worker_cancellation(
    accessor: &wasmtime::component::Accessor<ActorStore>,
    worker: WorkerId,
    active_workers: &HashSet<WorkerId>,
    cancelled_workers: &mut HashSet<WorkerId>,
) -> Result<bool> {
    if !active_workers.contains(&worker) || !cancelled_workers.insert(worker) {
        return Ok(false);
    }
    let requested =
        accessor.with(|mut access| access.get().worker_cancellations.request(worker))?;
    ensure!(
        requested,
        "component worker {} cancellation state diverged",
        worker.raw()
    );
    Ok(true)
}

const fn component_system_capacity(config: &ComponentActorConfig) -> usize {
    config
        .max_workers
        .saturating_mul(2)
        .saturating_add(config.command_capacity)
}

fn component_dispatch_can_start(
    dispatcher_busy: bool,
    event_observer: &ComponentActorEventObserver,
) -> bool {
    !dispatcher_busy && event_observer.has_ordinary_capacity(1)
}

fn completion_to_guest(completion: &ActorCompletion) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;
    let operation = completion.operation.sequence();
    let completion = match &completion.kind {
        ActorCompletionKind::Read { bytes } => abi::Completion::Read(abi::ReadCompleted {
            operation,
            bytes: bytes.clone(),
        }),
        ActorCompletionKind::Write { bytes_written } => {
            abi::Completion::Write(abi::WriteCompleted {
                operation,
                bytes_written: *bytes_written,
            })
        }
        ActorCompletionKind::Cancelled => abi::Completion::Cancelled(operation),
        ActorCompletionKind::Failed { message } => abi::Completion::Failed(abi::OperationFailed {
            operation,
            message: message.clone(),
        }),
    };
    abi::Event::Completion(completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_pending_observer_record_does_not_starve_the_next_dispatch() {
        let (sender, _receiver) = mpsc::channel(8);
        let observer = ComponentActorEventObserver::new(sender, None);
        assert!(
            observer
                .try_send(ComponentActorEvent::DispatcherReturned)
                .is_ok()
        );

        assert!(component_dispatch_can_start(false, &observer));
        assert!(!component_dispatch_can_start(true, &observer));
    }

    #[test]
    fn saturated_component_command_lane_returns_the_unadmitted_event() {
        let (sender, _receiver) = mpsc::channel(1);
        assert!(
            sender
                .try_send(RuntimeCommand::Presentation {
                    events: vec![(
                        PresentationEvent::PaneFocused {
                            pane: 1,
                            focused: true,
                        },
                        Some(1),
                    )],
                })
                .is_ok()
        );
        let pending = PresentationEvent::Key {
            pane: 1,
            physical_key: 42,
            logical_key: "x".to_string(),
            pressed: true,
            repeat: false,
            modifiers: 0,
        };
        let error = sender.try_send(RuntimeCommand::Presentation {
            events: vec![(pending, Some(2))],
        });
        let returned_events = match &error {
            Err(mpsc::error::TrySendError::Full(RuntimeCommand::Presentation { events })) => {
                Some(events.as_slice())
            }
            _ => None,
        };
        assert!(matches!(
            returned_events,
            Some([(
                PresentationEvent::Key {
                    physical_key: 42,
                    ..
                },
                Some(2)
            )])
        ));
        drop(error);
        assert_eq!(sender.capacity(), 0);
    }

    #[test]
    fn presentation_envelope_rejects_oversize_transactionally() {
        let (sender, mut receiver) = mpsc::channel(2);
        let make_events = |count: u64| {
            (0..count)
                .map(|index| {
                    (
                        PresentationEvent::Text {
                            pane: 7,
                            text: index.to_string(),
                        },
                        Some(index),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert!(matches!(
            ComponentActor::try_send_presentation_envelope(&sender, make_events(17)),
            Err(ComponentActorPresentationEnvelopeSendError::InvalidSize(events))
                if events.len() == 17
        ));
        assert!(receiver.try_recv().is_err());
        assert!(ComponentActor::try_send_presentation_envelope(&sender, make_events(16)).is_ok());
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::Presentation { events }) if events.len() == 16
        ));
    }

    #[test]
    fn critical_presentation_queue_never_exceeds_admission_capacity() {
        let mut queue = VecDeque::new();
        for physical_key in 0..128 {
            if queue.len() == 8 {
                assert!(matches!(
                    queue.pop_front(),
                    Some(QueuedPresentationEvent {
                        event: PresentationEvent::Key { .. },
                        ..
                    })
                ));
            }
            enqueue_presentation_dispatch(
                &mut queue,
                8,
                PresentationEvent::Key {
                    pane: 7,
                    physical_key,
                    logical_key: physical_key.to_string(),
                    pressed: true,
                    repeat: false,
                    modifiers: 0,
                },
            );
            assert!(queue.len() <= 8);
        }
    }

    #[test]
    fn presentation_envelope_reserves_its_whole_expansion_before_receive() {
        assert!(presentation_command_lane_has_capacity(496, 512));
        assert!(!presentation_command_lane_has_capacity(497, 512));
        assert!(!presentation_command_lane_has_capacity(511, 512));
        assert!(!presentation_command_lane_has_capacity(512, 512));

        let mut queue = (0..496_u64)
            .map(|watermark| QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 7,
                    text: watermark.to_string(),
                },
                context: DispatchContext {
                    event_watermark: Some(watermark),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            })
            .collect::<VecDeque<_>>();
        for physical_key in 496..512_u32 {
            enqueue_presentation_dispatch_with_watermark(
                &mut queue,
                512,
                PresentationEvent::Key {
                    pane: 7,
                    physical_key,
                    logical_key: physical_key.to_string(),
                    pressed: true,
                    repeat: false,
                    modifiers: 0,
                },
                Some(u64::from(physical_key)),
            );
        }
        assert_eq!(queue.len(), 512);
        assert_eq!(
            queue
                .front()
                .and_then(|queued| queued.context.event_watermark),
            Some(0)
        );
        assert_eq!(
            queue
                .back()
                .and_then(|queued| queued.context.event_watermark),
            Some(511)
        );
    }

    #[test]
    fn full_queue_evicts_only_coalescible_motion_before_reliable_input() {
        let mut queue = VecDeque::from([
            QueuedPresentationEvent {
                event: PresentationEvent::PointerMoved {
                    pane: 7,
                    x: 1.0,
                    y: 2.0,
                },
                context: DispatchContext {
                    event_watermark: Some(1),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            },
            QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 7,
                    text: "kept".to_owned(),
                },
                context: DispatchContext {
                    event_watermark: Some(2),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            },
        ]);
        enqueue_presentation_dispatch_with_watermark(
            &mut queue,
            2,
            PresentationEvent::Key {
                pane: 7,
                physical_key: 36,
                logical_key: "j".to_owned(),
                pressed: true,
                repeat: true,
                modifiers: 0,
            },
            Some(3),
        );
        assert_eq!(queue.len(), 2);
        assert!(matches!(
            queue.front().map(|queued| &queued.event),
            Some(PresentationEvent::Text { text, .. }) if text == "kept"
        ));
        assert!(matches!(
            queue.back().map(|queued| &queued.event),
            Some(PresentationEvent::Key { .. })
        ));
        assert_eq!(
            queue
                .front()
                .and_then(|queued| queued.context.event_watermark),
            Some(2)
        );
        assert_eq!(
            queue
                .back()
                .and_then(|queued| queued.context.event_watermark),
            Some(3)
        );
    }

    #[test]
    fn actor_event_observer_wakes_only_after_queueing_work() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_wakes = Arc::clone(&wakes);
        let (sender, mut receiver) = mpsc::channel(6);
        let observer = ComponentActorEventObserver::new(
            sender,
            Some(ComponentActorEventWaker::new(move || {
                observed_wakes.fetch_add(1, Ordering::Relaxed);
            })),
        );

        for worker in 1..=4 {
            assert!(
                observer
                    .try_send(ComponentActorEvent::WorkerStarted {
                        worker: WorkerId(worker)
                    })
                    .is_ok()
            );
        }
        assert!(matches!(
            observer.try_send(ComponentActorEvent::WorkerStarted {
                worker: WorkerId(5)
            }),
            Err(mpsc::error::TrySendError::Full(
                ComponentActorEvent::WorkerStarted {
                    worker: WorkerId(5)
                }
            ))
        ));
        assert!(
            observer
                .try_send(ComponentActorEvent::DispatcherFailed {
                    message: "failed".to_string()
                })
                .is_ok()
        );
        assert!(observer.try_send(ComponentActorEvent::Stopped).is_ok());
        assert_eq!(sender_capacity(&observer), 0);
        for worker in 1..=4 {
            assert_eq!(
                receiver.try_recv().ok(),
                Some(ComponentActorEvent::WorkerStarted {
                    worker: WorkerId(worker)
                })
            );
        }
        assert_eq!(
            receiver.try_recv().ok(),
            Some(ComponentActorEvent::DispatcherFailed {
                message: "failed".to_string()
            })
        );
        assert_eq!(receiver.try_recv().ok(), Some(ComponentActorEvent::Stopped));

        drop(receiver);
        assert!(observer.try_send(ComponentActorEvent::Stopped).is_err());
        assert_eq!(wakes.load(Ordering::Relaxed), 6);
    }

    fn sender_capacity(observer: &ComponentActorEventObserver) -> usize {
        observer.sender.capacity()
    }

    #[test]
    fn explicit_shutdown_ignores_only_errors_that_race_actor_teardown() {
        assert!(component_loop_result(Ok(()), false).is_ok());
        assert!(
            component_loop_result(Err(anyhow::anyhow!("invalid queued command")), true).is_ok()
        );
        assert_eq!(
            component_loop_result(Err(anyhow::anyhow!("invalid live command")), false)
                .err()
                .map(|error| error.to_string()),
            Some("invalid live command".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_replacement_is_atomic_and_rejects_symlink_destinations() -> Result<()> {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };

        let temporary = tempfile::tempdir()?;
        let destination = temporary.path().join("tracked.txt");
        fs::write(&destination, b"old contents")?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o640))?;
        let root = Dir::open_ambient_dir(temporary.path(), ambient_authority())?;

        let written = worker_replace_file(
            &root,
            true,
            PathBuf::from("tracked.txt"),
            b"new contents".to_vec(),
        )?;
        ensure!(written == 12, "unexpected replacement byte count");
        ensure!(
            fs::read(&destination)? == b"new contents",
            "replacement contents differ"
        );
        ensure!(
            fs::metadata(&destination)?.permissions().mode() & 0o777 == 0o640,
            "replacement did not preserve permissions"
        );
        ensure!(
            fs::read_dir(temporary.path())?
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".brood-replace-")),
            "replacement left a temporary file behind"
        );

        symlink("tracked.txt", temporary.path().join("link.txt"))?;
        ensure!(
            worker_replace_file(
                &root,
                true,
                PathBuf::from("link.txt"),
                b"must not follow".to_vec(),
            )
            .is_err(),
            "replacement followed a symlink destination"
        );
        ensure!(
            worker_replace_file(
                &root,
                false,
                PathBuf::from("tracked.txt"),
                b"must not write".to_vec(),
            )
            .is_err(),
            "replacement ignored the read-only grant"
        );
        ensure!(
            fs::read(&destination)? == b"new contents",
            "rejected replacement changed the destination"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn worker_regular_read_accepts_read_only_files_and_rejects_special_targets() -> Result<()> {
        use std::{
            fs,
            os::unix::{
                fs::{PermissionsExt, symlink},
                net::UnixListener,
            },
        };

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("readonly.txt");
        fs::write(&source, b"read-only contents")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o444))?;
        fs::create_dir(temporary.path().join("directory"))?;
        symlink("readonly.txt", temporary.path().join("link.txt"))?;
        let _socket = UnixListener::bind(temporary.path().join("socket"))?;
        let root = Dir::open_ambient_dir(temporary.path(), ambient_authority())?;

        ensure!(
            read_worker_regular_file(&root, PathBuf::from("readonly.txt"), 5, 4)? == b"only",
            "regular worker read returned the wrong slice"
        );
        for rejected in ["directory", "link.txt", "socket"] {
            ensure!(
                read_worker_regular_file(&root, PathBuf::from(rejected), 0, 16).is_err(),
                "regular worker read accepted {rejected}"
            );
        }
        Ok(())
    }

    #[test]
    fn presentation_backlog_retains_reliable_text_until_reserved_capacity_drains() {
        const CAPACITY: usize = 32;
        let mut queue = VecDeque::new();
        for index in 0..17 {
            enqueue_presentation_dispatch(
                &mut queue,
                CAPACITY,
                PresentationEvent::Text {
                    pane: 1,
                    text: index.to_string(),
                },
            );
        }
        assert!(!presentation_command_lane_has_capacity(
            queue.len(),
            CAPACITY
        ));
        let retained = PresentationEvent::Text {
            pane: 1,
            text: "retained".to_string(),
        };
        assert!(matches!(
            queue.pop_front(),
            Some(QueuedPresentationEvent {
                event: PresentationEvent::Text { ref text, .. },
                ..
            }) if text == "0"
        ));
        assert!(presentation_command_lane_has_capacity(
            queue.len(),
            CAPACITY
        ));
        enqueue_presentation_dispatch(&mut queue, CAPACITY, retained);
        assert_eq!(queue.len(), 17);
        assert!(matches!(
            queue.back().map(|queued| &queued.event),
            Some(PresentationEvent::Text { text, .. }) if text == "retained"
        ));
    }

    #[test]
    fn presentation_backlog_rejects_capacity_below_one_maximum_envelope() {
        assert!(validate_presentation_backlog(15).is_err());
        assert!(validate_presentation_backlog(16).is_ok());
    }

    #[test]
    fn presentation_backlog_evicts_motion_before_text() {
        let mut queue = VecDeque::from([
            QueuedPresentationEvent {
                event: PresentationEvent::PointerMoved {
                    pane: 1,
                    x: 1.0,
                    y: 2.0,
                },
                context: DispatchContext::default(),
            },
            QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 1,
                    text: "a".to_string(),
                },
                context: DispatchContext::default(),
            },
        ]);

        enqueue_presentation_dispatch(
            &mut queue,
            2,
            PresentationEvent::Text {
                pane: 1,
                text: "b".to_string(),
            },
        );

        assert_eq!(
            queue
                .into_iter()
                .map(|queued| queued.event)
                .collect::<Vec<_>>(),
            vec![
                PresentationEvent::Text {
                    pane: 1,
                    text: "a".to_string(),
                },
                PresentationEvent::Text {
                    pane: 1,
                    text: "b".to_string(),
                },
            ]
        );
    }

    #[test]
    fn presentation_backlog_retains_only_the_latest_consecutive_resize() {
        let mut queue = VecDeque::from([QueuedPresentationEvent {
            event: PresentationEvent::PaneResized {
                pane: 7,
                width: 640.0,
                height: 480.0,
                scale: 1.0,
                resize_epoch: Some(41),
            },
            context: DispatchContext::default(),
        }]);

        enqueue_presentation_dispatch(
            &mut queue,
            1,
            PresentationEvent::PaneResized {
                pane: 7,
                width: 1280.0,
                height: 720.0,
                scale: 1.25,
                resize_epoch: Some(42),
            },
        );
        assert_eq!(
            queue
                .into_iter()
                .map(|queued| queued.event)
                .collect::<Vec<_>>(),
            [PresentationEvent::PaneResized {
                pane: 7,
                width: 1280.0,
                height: 720.0,
                scale: 1.25,
                resize_epoch: Some(42),
            }]
        );
    }

    #[test]
    fn presentation_batching_is_opt_in_and_hard_capped() {
        assert_eq!(normalize_presentation_batch_limit(0), 1);
        assert_eq!(normalize_presentation_batch_limit(1), 1);
        assert_eq!(normalize_presentation_batch_limit(8), 8);
        assert_eq!(normalize_presentation_batch_limit(u8::MAX), 16);

        let mut queue = VecDeque::from([
            QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 1,
                    text: "a".to_owned(),
                },
                context: DispatchContext {
                    event_watermark: Some(1),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            },
            QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 1,
                    text: "b".to_owned(),
                },
                context: DispatchContext {
                    event_watermark: Some(2),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            },
        ]);
        let popped = pop_presentation_dispatch(&mut queue, 1);
        assert!(matches!(
            &popped,
            Ok(Some((
                bindings::exports::brood::plugin::actor::Event::Presentation(_),
                context
            ))) if context.event_watermark == Some(1)
        ));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn presentation_batch_preserves_fifo_cutoff_and_keeps_hints_singleton() {
        use bindings::exports::brood::plugin::actor as abi;

        let mut queue = (1..=20_u64)
            .map(|watermark| QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 1,
                    text: watermark.to_string(),
                },
                context: DispatchContext {
                    event_watermark: Some(watermark),
                    render_token: None,
                    presentation_events: 1,
                    ..DispatchContext::default()
                },
            })
            .collect::<VecDeque<_>>();
        queue.push_back(QueuedPresentationEvent {
            event: PresentationEvent::RenderHint(RenderHint {
                token: RenderToken(21),
                target_presentation_time_ns: 30,
                submission_deadline_ns: 29,
                relevant_panes: vec![1],
                event_watermark: 20,
            }),
            context: DispatchContext {
                event_watermark: Some(20),
                render_token: Some(RenderToken(21)),
                presentation_events: 1,
                ..DispatchContext::default()
            },
        });
        queue.push_back(QueuedPresentationEvent {
            event: PresentationEvent::Text {
                pane: 1,
                text: "later".to_owned(),
            },
            context: DispatchContext {
                event_watermark: Some(22),
                render_token: None,
                presentation_events: 1,
                ..DispatchContext::default()
            },
        });

        let first = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            &first,
            Ok(Some((abi::Event::PresentationBatch(events), context)))
                if events.len() == 16
                    && context.event_watermark == Some(16)
                    && context.presentation_events == 16
        ));
        let second = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            &second,
            Ok(Some((abi::Event::PresentationBatch(events), context)))
                if events.len() == 4
                    && context.event_watermark == Some(20)
                    && context.presentation_events == 4
        ));
        let hint = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            &hint,
            Ok(Some((
                abi::Event::Presentation(abi::PresentationEvent::RenderHint(_)),
                context
            ))) if context.render_token == Some(RenderToken(21))
                && context.presentation_events == 1
        ));
        let later = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            &later,
            Ok(Some((
                abi::Event::Presentation(abi::PresentationEvent::Text(text)),
                context
            ))) if text.text == "later" && context.event_watermark == Some(22)
        ));
    }

    #[test]
    fn large_presentation_payloads_are_singleton_batch_boundaries() {
        use bindings::exports::brood::plugin::actor as abi;

        let mut queue = VecDeque::from([
            QueuedPresentationEvent {
                event: PresentationEvent::Text {
                    pane: 1,
                    text: "before".to_owned(),
                },
                context: DispatchContext::default(),
            },
            QueuedPresentationEvent {
                event: PresentationEvent::Paste {
                    pane: 1,
                    text: "large".repeat(1024),
                },
                context: DispatchContext::default(),
            },
            QueuedPresentationEvent {
                event: PresentationEvent::Image {
                    pane: 1,
                    name: "image".to_owned(),
                    mime_type: "image/rgba".to_owned(),
                    bytes: vec![0; 1024],
                    preview_width: 1,
                    preview_height: 1,
                    preview_rgba8: vec![0; 4],
                },
                context: DispatchContext::default(),
            },
        ]);
        let text = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            text,
            Ok(Some((
                abi::Event::Presentation(abi::PresentationEvent::Text(_)),
                _
            )))
        ));
        let paste = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            paste,
            Ok(Some((
                abi::Event::Presentation(abi::PresentationEvent::Paste(_)),
                _
            )))
        ));
        let image = pop_presentation_dispatch(&mut queue, 16);
        assert!(matches!(
            image,
            Ok(Some((
                abi::Event::Presentation(abi::PresentationEvent::Image(_)),
                _
            )))
        ));
    }

    #[test]
    fn coalesced_pointer_motion_retains_the_highest_reduced_watermark() {
        let mut queue = VecDeque::new();
        enqueue_presentation_dispatch_with_watermark(
            &mut queue,
            8,
            PresentationEvent::PointerMoved {
                pane: 1,
                x: 1.0,
                y: 1.0,
            },
            Some(9),
        );
        enqueue_presentation_dispatch_with_watermark(
            &mut queue,
            8,
            PresentationEvent::PointerMoved {
                pane: 1,
                x: 2.0,
                y: 2.0,
            },
            Some(7),
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue
                .front()
                .and_then(|queued| queued.context.event_watermark),
            Some(9)
        );
        assert!(matches!(
            queue.front().map(|queued| &queued.event),
            Some(PresentationEvent::PointerMoved { x: 2.0, y: 2.0, .. })
        ));
    }
}
