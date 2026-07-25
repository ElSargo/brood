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

use crate::completion_io::{
    ActorCompletion, ActorCompletionKind, CompletionIo, CompletionIoConfig, CompletionIoError,
    FilesystemGrant, OperationId, start_completion_io as start_shared_completion_io,
};
use crate::coordination::{
    ActorWatchResult, CoordinationConfig, CoordinationHub, NotificationError, NotificationId,
    PollError, PollableId, WaitMode,
};
use crate::filesystem_watch::{
    FilesystemWatchBatchEvent, FilesystemWatchError, FilesystemWatchEventKind, FilesystemWatchHub,
    FilesystemWatchId, NativeFilesystemWatcherService,
};
use crate::lifecycle::wait_for_lifecycle_reply;
use crate::lifecycle::{PluginLifecycleEvent, PluginStartReason, PluginStopReason};
use crate::presentation::{
    FontIntent, FontWeight, PresentationError, PresentationEvent, PresentationHost,
    SessionCommandRequest, SessionEvent, SessionOperation, TextLayoutOptions,
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
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc as std_mpsc,
    },
    thread,
};

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{FileTypeExt, OpenOptionsSyncExt};
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

static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

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
    },
    ActorStopRequested {
        id: String,
    },
    AppStartRequested {
        id: String,
        argv: Vec<String>,
    },
    SessionMessageRequested {
        session_id: u64,
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

/// Cumulative delivery counters and the configured capacity for an actor's
/// global mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentActorMailboxStats {
    pub enqueued_messages: u64,
    pub dropped_messages: u64,
    pub capacity: usize,
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
    /// Pointer motion may coalesce; key, text, button, focus, and resize
    /// events are never silently discarded.
    pub presentation_backlog: usize,
    pub mailbox_backlog: usize,
    pub max_workers: usize,
    pub max_outstanding_io: usize,
    pub max_completion_bytes: usize,
    pub max_notifications: usize,
    pub max_pollables: usize,
    pub max_wait_set: usize,
    /// Generation-owned process, PTY, socket, stream, and timer policy.
    pub runtime_io: RuntimeIoConfig,
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
    /// Admit explicitly addressed session-to-presentation actor messages.
    /// Presentation actors never receive this cross-placement capability.
    pub presentation_messaging: bool,
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
            max_notifications: 64,
            max_pollables: 256,
            max_wait_set: 64,
            runtime_io: RuntimeIoConfig::default(),
            presentation: None,
            actor_management: false,
            app_management: false,
            session_messaging: false,
            presentation_messaging: false,
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
) -> Result<(
    CompletionIo,
    mpsc::Receiver<ActorCompletion>,
    JoinHandle<()>,
)> {
    start_shared_completion_io(
        generation,
        &CompletionIoConfig {
            filesystem: config.filesystem.clone(),
            max_outstanding: config.max_outstanding_io,
            max_completion_bytes: config.max_completion_bytes,
        },
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
    event_observer: mpsc::Sender<ComponentActorEvent>,
    shutdown_requested: Arc<AtomicBool>,
    limits: GenerationStoreLimits,
}

#[derive(Clone)]
struct WorkerFilesystem {
    root: Arc<Dir>,
    writable: bool,
    max_bytes: usize,
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
    fn graphics_submit_draw_packet(
        &mut self,
        pane: u64,
        packet: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<u64, bindings::brood::plugin::host::GraphicsError>>
    {
        Ok(self
            .presentation
            .as_ref()
            .ok_or(PresentationError::Unavailable)
            .and_then(|presentation| {
                presentation.submit_draw_packet(self.actor_generation, pane, packet)
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
    pending_observer: Option<mpsc::Sender<ComponentActorEvent>>,
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
        PresentationError::Unavailable => AbiError::Unavailable,
        PresentationError::InvalidPane => AbiError::InvalidPane,
        PresentationError::InvalidArgument => AbiError::InvalidArgument,
        PresentationError::LimitReached => AbiError::LimitReached,
        PresentationError::InvalidPacket => AbiError::InvalidPacket,
        PresentationError::InvalidResource => AbiError::InvalidResource,
        PresentationError::ResourceInUse => AbiError::ResourceInUse,
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
    Presentation(PresentationEvent),
    Session(SessionEvent),
    Shutdown,
}

/// Handle for one live component actor.
///
/// [`Self::shutdown`] joins the runtime and I/O supervisor and waits for
/// tracked blocking I/O. [`Drop`] aborts the owned async handles as a fallback;
/// blocking-pool work which has already started cannot be preempted.
pub struct ComponentActor {
    generation: u64,
    completion_io: CompletionIo,
    coordination: CoordinationHub,
    filesystem_watches: FilesystemWatchHub,
    runtime_io: RuntimeIoHub,
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
    byte_budget: GenerationByteBudget,
    observer_result_bytes: Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
}

impl ComponentActor {
    /// Load and start a Component Model actor.
    pub async fn load(path: &Path, config: ComponentActorConfig) -> Result<Self> {
        ensure!(
            config.command_capacity > 0,
            "command capacity must be non-zero"
        );
        ensure!(
            config.presentation_backlog > 0,
            "presentation backlog must be non-zero"
        );
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

        let generation = NEXT_ACTOR_GENERATION
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
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
        let runtime_io =
            RuntimeIoHub::new(generation, config.runtime_io.clone(), coordination.clone())
                .map_err(|error| anyhow::anyhow!("create component runtime I/O hub: {error}"))?;

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
        let (completion_io, io_completion_rx, io_task) = start_completion_io(generation, &config)?;
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let mailbox_slots = Arc::new(Semaphore::new(config.mailbox_backlog));
        let mailbox_capacity = config.mailbox_backlog;
        let byte_budget = GenerationByteBudget::new(config.limits.generation_host_bytes);
        let observer_result_bytes = Arc::new(Mutex::new(HashMap::new()));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = mpsc::channel(config.command_capacity);
        let actor_filesystem_watches = filesystem_watches.clone();
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
                event_observer: event_tx.clone(),
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
        let plugin =
            match bindings::BroodPlugin::instantiate_async(&mut store, &component, &linker).await {
                Ok(plugin) => plugin,
                Err(error) => {
                    completion_io.stop();
                    let _ = io_task.await;
                    return Err(anyhow::anyhow!("instantiate component actor: {error}"));
                }
            };

        let (completion_tx, completion_rx) = mpsc::channel(config.max_outstanding_io);
        let runtime_completion_io = completion_io.clone();
        let runtime_byte_budget = byte_budget.clone();
        let runtime_observer_result_bytes = Arc::clone(&observer_result_bytes);
        let limits = config.limits;
        let runtime_task = tokio::spawn(async move {
            let result = run_component_actor(
                &mut store,
                plugin,
                command_rx,
                io_completion_rx,
                notification_rx,
                completion_tx,
                event_tx,
                config,
                runtime_byte_budget,
                runtime_observer_result_bytes,
            )
            .await;
            runtime_completion_io.stop();
            result
        });

        Ok(Self {
            generation,
            completion_io,
            coordination,
            filesystem_watches: actor_filesystem_watches,
            runtime_io,
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
            byte_budget,
            observer_result_bytes,
        })
    }

    pub const fn generation(&self) -> u64 {
        self.generation
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
        self.coordination.stats()
    }

    pub fn filesystem_watch_stats(&self) -> crate::filesystem_watch::FilesystemWatchStats {
        self.filesystem_watches.stats()
    }

    pub fn runtime_io_stats(&self) -> crate::runtime_io::RuntimeIoStats {
        self.runtime_io.stats()
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
            &self.byte_budget,
        )
    }

    /// Deliver one typed pane lifecycle or input event without blocking.
    pub fn try_send_presentation_event(&self, event: PresentationEvent) -> Result<()> {
        self.commands
            .try_send(RuntimeCommand::Presentation(event))
            .map_err(|error| {
                anyhow::anyhow!("component actor rejected presentation event: {error}")
            })
    }

    /// Deliver one typed session snapshot or command completion without
    /// blocking the presentation event loop.
    pub fn try_send_session_event(&self, event: SessionEvent) -> Result<()> {
        self.commands
            .try_send(RuntimeCommand::Session(event))
            .map_err(|error| anyhow::anyhow!("component actor rejected session event: {error}"))
    }

    /// Deliver lifecycle feedback for one child owned by a blessed init actor.
    pub fn try_send_child_event(&self, event: ComponentChildEvent) -> Result<()> {
        self.commands
            .try_send(RuntimeCommand::Child(event))
            .map_err(|error| anyhow::anyhow!("component init actor rejected child event: {error}"))
    }

    /// Deliver app lifecycle feedback to a blessed session init actor.
    pub fn try_send_app_event(&self, event: ComponentAppEvent) -> Result<()> {
        self.commands
            .try_send(RuntimeCommand::App(event))
            .map_err(|error| anyhow::anyhow!("component init actor rejected app event: {error}"))
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
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
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

    /// Observe the completion lane independently of normal mailbox traffic.
    pub async fn recv_completion(&mut self) -> Option<ActorCompletion> {
        self.completions.recv().await
    }

    fn release_observer_result_bytes(&self, event: &ComponentActorEvent) {
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
        self.runtime_io.teardown().await;
        self.coordination.stop();
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

fn increment_saturating(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_add(1))
    });
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
    match commands.try_send(RuntimeCommand::Mailbox {
        sender,
        body,
        permit,
        byte_permit,
    }) {
        Ok(()) => {
            increment_saturating(mailbox_enqueued);
            Ok(())
        }
        Err(error) => {
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
        self.coordination.stop();
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

struct EpochTicker {
    stop: std_mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, interval: Duration) -> Result<Self> {
        let (stop, stopped) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("brood-component-epoch".to_string())
            .spawn(move || {
                while stopped.recv_timeout(interval) == Err(std_mpsc::RecvTimeoutError::Timeout) {
                    engine.increment_epoch();
                }
            })
            .context("start component epoch ticker")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
            })
        })
        .transpose()
}

type GuestActor = bindings::exports::brood::plugin::actor::Guest;
type GuestEvent = bindings::exports::brood::plugin::actor::Event;
type GuestCommand = bindings::exports::brood::plugin::actor::Command;

enum GuestCallOutcome {
    Dispatch(wasmtime::Result<Vec<GuestCommand>>),
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

async fn run_component_actor(
    store: &mut Store<ActorStore>,
    plugin: bindings::BroodPlugin,
    mut commands: mpsc::Receiver<RuntimeCommand>,
    mut io_completions: mpsc::Receiver<ActorCompletion>,
    mut notifications: mpsc::Receiver<NotificationId>,
    completion_observer: mpsc::Sender<ActorCompletion>,
    event_observer: mpsc::Sender<ComponentActorEvent>,
    config: ComponentActorConfig,
    byte_budget: GenerationByteBudget,
    observer_result_bytes: Arc<Mutex<HashMap<WorkerId, GenerationBytePermit>>>,
) -> Result<()> {
    let guest = plugin.brood_plugin_actor();
    let stopped_observer = event_observer.clone();
    let blocking_io = store.data().blocking_io.clone();
    let concurrent_result = store
        .run_concurrent(async move |accessor| {
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
        })
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
        Ok(result) => result,
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

async fn run_component_event_loop(
    accessor: &wasmtime::component::Accessor<ActorStore>,
    guest: &GuestActor,
    commands: &mut mpsc::Receiver<RuntimeCommand>,
    io_completions: &mut mpsc::Receiver<ActorCompletion>,
    notifications: &mut mpsc::Receiver<NotificationId>,
    completion_observer: &mpsc::Sender<ActorCompletion>,
    event_observer: &mpsc::Sender<ComponentActorEvent>,
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

    loop {
        if !dispatcher_busy {
            let event = pop_component_dispatch(
                &mut dispatch_lane,
                &mut completion_queue,
                &mut notification_queue,
                &mut system_queue,
                &mut presentation_queue,
                &mut mailbox_queue,
                accessor,
                event_observer,
            )?;
            if let Some((event, mailbox_permit, byte_permit)) = event {
                dispatcher_busy = true;
                let call = accessor
                    .with(|mut access| dispatch.start_call_concurrent(&mut access, (event,)))?;
                calls.push(
                    async move {
                        // Admission remains held until the accepted mailbox
                        // message has reached its terminal dispatch result.
                        let _mailbox_permit = mailbox_permit;
                        let _byte_permit = byte_permit;
                        let result = dispatch
                            .finish_call_concurrent(accessor, call)
                            .await
                            .map(|(commands,)| commands);
                        GuestCallOutcome::Dispatch(result)
                    }
                    .boxed(),
                );
            }
        }

        tokio::select! {
            command = commands.recv() => match command {
                Some(RuntimeCommand::Mailbox { sender, body, permit, byte_permit }) => {
                    // The sender acquired one of exactly `mailbox_backlog`
                    // permits before the command was accepted, so this queue
                    // cannot overflow or silently discard the message.
                    mailbox_queue.push_back((
                        abi::Event::Mailbox(abi::MailboxMessage { sender, body }),
                        permit,
                        byte_permit,
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
                    )?;
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
                    )?;
                    pending_lifecycle = Some(reply);
                }
                Some(RuntimeCommand::Child(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_child_to_guest(event),
                    )?;
                }
                Some(RuntimeCommand::App(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_app_to_guest(event),
                    )?;
                }
                Some(RuntimeCommand::Presentation(event)) => {
                    enqueue_presentation_dispatch(
                        &mut presentation_queue,
                        config.presentation_backlog,
                        event,
                    )?;
                }
                Some(RuntimeCommand::Session(event)) => {
                    enqueue_dispatch(
                        &mut system_queue,
                        component_system_capacity(config),
                        component_session_to_guest(event),
                    )?;
                }
                Some(RuntimeCommand::Shutdown) | None => break,
            },
            completion = io_completions.recv(), if completion_queue.len() < config.max_outstanding_io => {
                let Some(completion) = completion else { continue; };
                enqueue_dispatch(
                    &mut completion_queue,
                    config.max_outstanding_io,
                    completion_to_guest(&completion),
                )?;
                let _ = completion_observer.try_send(completion.clone());
                let _ = event_observer.try_send(ComponentActorEvent::CompletionDelivered(completion));
            }
            notification = notifications.recv(), if notification_queue.len() < config.max_notifications => {
                let Some(notification) = notification else { continue; };
                notification_queue.push_back(notification);
            }
            outcome = calls.next(), if !calls.is_empty() => {
                let Some(outcome) = outcome else { continue; };
                match outcome {
                    GuestCallOutcome::Dispatch(result) => {
                        dispatcher_busy = false;
                        match result {
                            Ok(returned) => {
                                let _ = event_observer.try_send(ComponentActorEvent::DispatcherReturned);
                                ensure!(
                                    returned.len() <= config.command_capacity,
                                    "actor dispatcher returned too many commands"
                                );
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
                                                    )?;
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
                                            )?;
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
                                                    session_id: request.session_id,
                                                    actor: request.actor,
                                                    body: request.body,
                                                })
                                                .context("session actor message request queue is full")?;
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
                            )?;
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
                                    )?;
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
                                        )?;
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
                                )?;
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
                                    )?;
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
                                    )?;
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
                                )?;
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
    let event = match event {
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
        } => abi::PresentationEvent::PaneResized(abi::PaneResized {
            pane,
            width,
            height,
            scale,
        }),
        PresentationEvent::PaneFocused { pane, focused } => {
            abi::PresentationEvent::PaneFocused(abi::PaneFocused { pane, focused })
        }
        PresentationEvent::PointerMoved { pane, x, y } => {
            abi::PresentationEvent::PointerMoved(abi::PointerMoved { pane, x, y })
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
    };
    abi::Event::Presentation(event)
}

fn component_session_to_guest(event: SessionEvent) -> GuestEvent {
    use bindings::exports::brood::plugin::actor as abi;

    let event = match event {
        SessionEvent::Snapshot(snapshot) => abi::SessionEvent::Snapshot(abi::SessionSnapshot {
            revision: snapshot.revision,
            active: snapshot.active,
            sessions: snapshot
                .sessions
                .into_iter()
                .map(|session| abi::SessionSummary {
                    id: session.id,
                    name: session.name,
                    active: session.active,
                    windows: session.windows,
                })
                .collect(),
        }),
        SessionEvent::CommandCompleted(completion) => {
            abi::SessionEvent::CommandCompleted(abi::SessionCommandCompleted {
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
        request: request.request,
        operation,
        session: request.session,
        name: request.name,
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
    presentation_queue: &mut VecDeque<PresentationEvent>,
    mailbox_queue: &mut VecDeque<(GuestEvent, OwnedSemaphorePermit, GenerationBytePermit)>,
    accessor: &wasmtime::component::Accessor<ActorStore>,
    event_observer: &mpsc::Sender<ComponentActorEvent>,
) -> Result<
    Option<(
        GuestEvent,
        Option<OwnedSemaphorePermit>,
        Option<GenerationBytePermit>,
    )>,
> {
    use bindings::exports::brood::plugin::actor as abi;

    for _lane in 0..5 {
        let lane = *next_lane;
        *next_lane = (*next_lane + 1) % 5;
        let event = match lane {
            0 => completion_queue
                .pop_front()
                .map(|queued| (queued.event, None, None)),
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
                    ));
                    break;
                }
                selected
            }
            2 => system_queue
                .pop_front()
                .map(|queued| (queued.event, None, None)),
            3 => presentation_queue
                .pop_front()
                .map(component_presentation_to_guest)
                .map(|event| (event, None, None)),
            _ => mailbox_queue
                .pop_front()
                .map(|(event, permit, byte_permit)| (event, Some(permit), Some(byte_permit))),
        };
        if event.is_some() {
            return Ok(event);
        }
    }
    Ok(None)
}

fn enqueue_presentation_dispatch(
    queue: &mut VecDeque<PresentationEvent>,
    capacity: usize,
    event: PresentationEvent,
) -> Result<()> {
    if let PresentationEvent::PointerMoved { pane, .. } = &event {
        if let Some(queued) = queue.back_mut()
            && matches!(queued, PresentationEvent::PointerMoved { pane: queued_pane, .. } if *queued_pane == *pane)
        {
            *queued = event;
            return Ok(());
        }
        if queue.len() < capacity {
            queue.push_back(event);
        }
        return Ok(());
    }

    if queue.len() >= capacity {
        if let Some(index) = queue
            .iter()
            .position(|queued| matches!(queued, PresentationEvent::PointerMoved { .. }))
        {
            queue.remove(index);
        } else {
            anyhow::bail!("actor presentation dispatch backlog is full");
        }
    }
    queue.push_back(event);
    Ok(())
}

fn enqueue_dispatch(
    queue: &mut VecDeque<QueuedGuestEvent>,
    capacity: usize,
    event: GuestEvent,
) -> Result<()> {
    ensure!(queue.len() < capacity, "actor dispatch backlog is full");
    queue.push_back(QueuedGuestEvent {
        event,
        _byte_permit: None,
    });
    Ok(())
}

fn enqueue_retained_dispatch(
    queue: &mut VecDeque<QueuedGuestEvent>,
    capacity: usize,
    event: GuestEvent,
    byte_permit: GenerationBytePermit,
) -> Result<()> {
    ensure!(queue.len() < capacity, "actor dispatch backlog is full");
    queue.push_back(QueuedGuestEvent {
        event,
        _byte_permit: Some(byte_permit),
    });
    Ok(())
}

fn observe_retained_worker_event(
    observer: &mpsc::Sender<ComponentActorEvent>,
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
    event_observer: &mpsc::Sender<ComponentActorEvent>,
    config: &ComponentActorConfig,
    byte_permit: GenerationBytePermit,
) -> Result<bool> {
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
        )?;
        let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
        return Ok(false);
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
            )?;
            let _ = event_observer.try_send(ComponentActorEvent::WorkerFailed { worker, message });
            return Ok(false);
        }
    };
    enqueue_dispatch(
        system_queue,
        component_system_capacity(config),
        abi::Event::Worker(abi::WorkerEvent::Started(worker.raw())),
    )?;
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
    Ok(true)
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
    fn presentation_backlog_never_discards_text() {
        let mut queue = VecDeque::new();
        assert!(
            enqueue_presentation_dispatch(
                &mut queue,
                2,
                PresentationEvent::Text {
                    pane: 1,
                    text: "a".to_string(),
                },
            )
            .is_ok()
        );
        assert!(
            enqueue_presentation_dispatch(
                &mut queue,
                2,
                PresentationEvent::Text {
                    pane: 1,
                    text: "b".to_string(),
                },
            )
            .is_ok()
        );

        assert!(
            enqueue_presentation_dispatch(
                &mut queue,
                2,
                PresentationEvent::Text {
                    pane: 1,
                    text: "c".to_string(),
                },
            )
            .is_err()
        );
        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
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
    fn presentation_backlog_evicts_motion_before_text() {
        let mut queue = VecDeque::from([
            PresentationEvent::PointerMoved {
                pane: 1,
                x: 1.0,
                y: 2.0,
            },
            PresentationEvent::Text {
                pane: 1,
                text: "a".to_string(),
            },
        ]);

        assert!(
            enqueue_presentation_dispatch(
                &mut queue,
                2,
                PresentationEvent::Text {
                    pane: 1,
                    text: "b".to_string(),
                },
            )
            .is_ok()
        );

        assert_eq!(
            queue.into_iter().collect::<Vec<_>>(),
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
}
