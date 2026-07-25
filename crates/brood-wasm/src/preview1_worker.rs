//! WASI Preview 1 compatibility workers for stackful POSIX-style guest code.
//!
//! A `wasm32-wasip1-threads` module imports one shared linear memory. The
//! actor and every worker are separate instances in separate stores, so an
//! asynchronous Preview 1 syscall suspends only the store which issued it.
//! All call futures are nevertheless owned and polled by one parent Tokio
//! task; this runtime does not turn workers into native threads.
//!
//! Only linear memory is shared. A worker has store-local WASI descriptors,
//! clocks, random state, arguments, environment, globals, tables, and TLS.
//! Every WASI table is derived from one immutable actor capability grant, so a
//! worker cannot gain authority over its actor. Descriptor identity remains
//! store-local: a raw descriptor opened by the actor is not automatically
//! meaningful in a worker store.
//!
//! Guest modules must export the reservation ABI named by
//! [`WORKER_RESERVE_EXPORT`] and [`WORKER_RELEASE_EXPORT`]. The actor-side
//! implementation allocates worker stack and TLS slabs with the guest's own
//! allocator. The host never grows memory behind the allocator's back.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::mpsc as std_mpsc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use futures::{
    StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use tokio::{
    io::{AsyncRead, AsyncWriteExt, DuplexStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::{AbortHandle as TokioAbortHandle, JoinHandle as TokioJoinHandle},
};
use wasmtime::{Config, Engine, ExternType, Instance, Linker, Module, SharedMemory, Store, Val};
use wasmtime_wasi::{
    DirPerms, FilePerms, WasiCtxBuilder,
    cli::{AsyncStdinStream, IsTerminal, StdinStream},
    p1::{self, WasiP1Ctx},
};

use crate::completion_io::{
    ActorCompletion, ActorCompletionKind, CompletionIo, CompletionIoConfig, CompletionIoError,
    FilesystemGrant, OperationId, start_completion_io,
};
use crate::coordination::{
    ActorNotification, ActorWatchResult, CoordinationConfig, CoordinationHub, NotificationError,
    NotificationId, PollError, PollableId, WaitMode,
};
use crate::filesystem_watch::{
    FilesystemWatchError, FilesystemWatchHub, FilesystemWatchId, NativeFilesystemWatcherService,
};
use crate::lifecycle::{
    PluginLifecycleEvent, PluginStartReason, PluginStopReason, wait_for_lifecycle_reply,
};
use crate::resource_limits::{
    GenerationByteBudget, GenerationBytePermit, GenerationStoreLimits, WasmResourceLimits,
};
use crate::runtime_io::{
    ChildStatus, PtySpawnOptions, RuntimeIoCapabilities, RuntimeIoConfig, RuntimeIoError,
    RuntimeIoHub, StreamRead, TerminalProfile, TimerStatus,
};

#[allow(
    clippy::derive_partial_eq_without_eq,
    clippy::too_many_arguments,
    reason = "generated WITX bindings define the ABI shapes"
)]
mod host_abi {
    wiggle::from_witx!({
        witx: ["witx/brood_preview1.witx"],
        async: { brood_preview1::{worker_wait_any, worker_loopback_connect, child_drop, stream_drop, timer_drop} },
    });
}

impl wiggle::GuestErrorType for host_abi::types::Errno {
    fn success() -> Self {
        Self::Success
    }
}

/// Module containing the shared memory import used by `wasm32-wasip1-threads`.
pub const SHARED_MEMORY_IMPORT_MODULE: &str = "env";
/// Name of the shared memory import used by `wasm32-wasip1-threads`.
pub const SHARED_MEMORY_IMPORT_NAME: &str = "memory";
/// Actor export which reserves a disjoint worker stack and TLS slab.
pub const WORKER_RESERVE_EXPORT: &str = "brood_worker_reserve_v1";
/// Actor export which releases a completed worker's stack and TLS slab.
pub const WORKER_RELEASE_EXPORT: &str = "brood_worker_release_v1";
/// Marker export used to route a core module before legacy inventory discovery.
pub const COMPAT_MARKER_EXPORT: &str = "brood_preview1_compat_v1";
/// Fixed actor dispatcher export for mailbox and typed completion events.
pub const COMPAT_DISPATCH_EXPORT: &str = "brood_actor_dispatch_v1";
/// Fixed worker dispatcher export. `worker_spawn` only selects its argument.
pub const COMPAT_WORKER_EXPORT: &str = "brood_worker_run_v1";
/// Marker value returned by [`COMPAT_MARKER_EXPORT`].
pub const COMPAT_MARKER_MAGIC: u32 = 0x4d50_3101;

const EVENT_MAILBOX: u32 = 1;
const EVENT_COMPLETION_READ: u32 = 2;
const EVENT_COMPLETION_WRITE: u32 = 3;
const EVENT_COMPLETION_CANCELLED: u32 = 4;
const EVENT_COMPLETION_FAILED: u32 = 5;
const EVENT_WORKER_STARTED: u32 = 6;
const EVENT_WORKER_FINISHED: u32 = 7;
const EVENT_WORKER_FAILED: u32 = 8;
const EVENT_WORKER_CANCELLED: u32 = 9;
const EVENT_NOTIFICATION: u32 = 10;
const EVENT_LIFECYCLE_START: u32 = 11;
const EVENT_LIFECYCLE_STOP: u32 = 12;
static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

const STACK_ALIGNMENT: u32 = 16;

type WorkerInputRegistry = Arc<Mutex<HashMap<Preview1WorkerId, Option<mpsc::Sender<Vec<u8>>>>>>;

/// One filesystem capability inherited by the actor and all of its workers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preview1FilesystemGrant {
    /// Host directory opened by the runtime.
    pub host_path: PathBuf,
    /// Path at which the directory is visible to the guest.
    pub guest_path: String,
    /// Whether mutation is allowed in addition to reads.
    pub writable: bool,
}

/// Immutable capabilities used to derive every store-local WASI table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Preview1ActorCapabilities {
    /// Optional preopened directory shared as authority, not as descriptor state.
    pub filesystem: Option<Preview1FilesystemGrant>,
    /// Process, terminal, and loopback authority inherited by every worker.
    pub runtime_io: RuntimeIoCapabilities,
}

/// Runtime limits for Preview 1 compatibility workers.
#[derive(Clone, Copy, Debug)]
pub struct Preview1WorkerConfig {
    /// Maximum number of preparing or running workers.
    pub max_workers: usize,
    /// Maximum actor calls and worker preparations waiting for the actor store.
    pub max_pending_actor_operations: usize,
    /// Maximum completed I/O results reserved for actor dispatch.
    ///
    /// Once this lane is full the parent stops receiving from the completion
    /// supervisor, propagating backpressure without consuming general actor
    /// operation capacity.
    pub max_pending_completions: usize,
    /// Capacity of the public command channel.
    pub command_capacity: usize,
    /// Maximum mailbox events waiting for the actor dispatcher.
    pub mailbox_backlog: usize,
    /// Maximum outstanding completion-I/O operations.
    pub max_outstanding_io: usize,
    /// Maximum completion read/write payload.
    pub max_completion_bytes: usize,
    /// Maximum live capability-rooted filesystem watches.
    pub max_filesystem_watches: usize,
    /// Maximum queued filesystem events before a rescan marker replaces them.
    pub max_filesystem_watch_events: usize,
    /// Maximum live shared-memory notification resources.
    pub max_notifications: usize,
    /// Maximum live worker pollables.
    pub max_pollables: usize,
    /// Maximum pollables in one worker wait set.
    pub max_wait_set: usize,
    /// Byte capacity of each actor-private worker stdin channel.
    pub worker_input_capacity: usize,
    /// Whole-message backlog for each actor-private worker stdin channel.
    pub worker_input_backlog: usize,
    /// Stack bytes reserved through the guest allocator for each worker.
    pub worker_stack_bytes: u32,
    /// Wall-clock cadence of the Wasmtime engine epoch ticker.
    pub epoch_tick_interval: Duration,
    /// Epochs a CPU-running actor or internal allocator call may run before
    /// cooperatively yielding.
    pub actor_yield_ticks: u64,
    /// Epochs a CPU-running worker may run before cooperatively yielding.
    pub worker_yield_ticks: u64,
    /// Host and guest byte ceilings for one actor generation.
    pub limits: WasmResourceLimits,
}

impl Default for Preview1WorkerConfig {
    fn default() -> Self {
        Self {
            max_workers: 64,
            max_pending_actor_operations: 128,
            max_pending_completions: 1024,
            command_capacity: 128,
            mailbox_backlog: 128,
            max_outstanding_io: 64,
            max_completion_bytes: 1024 * 1024,
            max_filesystem_watches: 16,
            max_filesystem_watch_events: 4096,
            max_notifications: 64,
            max_pollables: 256,
            max_wait_set: 64,
            worker_input_capacity: 64 * 1024,
            worker_input_backlog: 16,
            worker_stack_bytes: 1024 * 1024,
            epoch_tick_interval: Duration::from_millis(2),
            actor_yield_ticks: 1,
            worker_yield_ticks: 1,
            limits: WasmResourceLimits::default(),
        }
    }
}

#[derive(Clone, Debug)]
enum CompatEvent {
    Mailbox {
        sender: String,
        payload: Vec<u8>,
    },
    Completion(ActorCompletion),
    WorkerStarted(Preview1WorkerId),
    WorkerFinished {
        worker: Preview1WorkerId,
        value: u32,
    },
    WorkerFailed {
        worker: Preview1WorkerId,
        message: String,
    },
    WorkerCancelled(Preview1WorkerId),
    Notification(ActorNotification),
    Lifecycle(PluginLifecycleEvent),
}

impl CompatEvent {
    const fn kind(&self) -> u32 {
        match self {
            Self::Mailbox { .. } => EVENT_MAILBOX,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Read { .. },
                ..
            }) => EVENT_COMPLETION_READ,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Write { .. },
                ..
            }) => EVENT_COMPLETION_WRITE,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Cancelled,
                ..
            }) => EVENT_COMPLETION_CANCELLED,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Failed { .. },
                ..
            }) => EVENT_COMPLETION_FAILED,
            Self::WorkerStarted(_) => EVENT_WORKER_STARTED,
            Self::WorkerFinished { .. } => EVENT_WORKER_FINISHED,
            Self::WorkerFailed { .. } => EVENT_WORKER_FAILED,
            Self::WorkerCancelled(_) => EVENT_WORKER_CANCELLED,
            Self::Notification(_) => EVENT_NOTIFICATION,
            Self::Lifecycle(PluginLifecycleEvent::Start(_)) => EVENT_LIFECYCLE_START,
            Self::Lifecycle(PluginLifecycleEvent::Stop(_)) => EVENT_LIFECYCLE_STOP,
        }
    }

    const fn sender(&self) -> &[u8] {
        match self {
            Self::Mailbox { sender, .. } => sender.as_bytes(),
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => context.instance_id.as_bytes(),
            Self::Lifecycle(PluginLifecycleEvent::Stop(context)) => context.instance_id.as_bytes(),
            _ => &[],
        }
    }

    fn payload(&self) -> &[u8] {
        match self {
            Self::Mailbox { payload, .. } => payload,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Read { bytes },
                ..
            }) => bytes,
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Failed { message },
                ..
            })
            | Self::WorkerFailed { message, .. } => message.as_bytes(),
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => {
                context.workspace_root.as_bytes()
            }
            _ => &[],
        }
    }

    const fn operation(&self) -> u64 {
        match self {
            Self::Completion(completion) => completion.operation.sequence(),
            Self::WorkerStarted(worker)
            | Self::WorkerCancelled(worker)
            | Self::WorkerFinished { worker, .. }
            | Self::WorkerFailed { worker, .. } => worker.raw(),
            Self::Notification(notification) => notification.notification.raw(),
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => context.generation,
            Self::Lifecycle(PluginLifecycleEvent::Stop(context)) => context.generation,
            Self::Mailbox { .. } => 0,
        }
    }

    const fn value(&self) -> u64 {
        match self {
            Self::Completion(ActorCompletion {
                kind: ActorCompletionKind::Write { bytes_written },
                ..
            }) => *bytes_written,
            Self::WorkerFinished { value, .. } => *value as u64,
            Self::Notification(notification) => notification.generation,
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => match context.reason {
                PluginStartReason::Initial => 1,
                PluginStartReason::Replacement => 2,
                PluginStartReason::SessionRebind => 3,
            },
            Self::Lifecycle(PluginLifecycleEvent::Stop(context)) => match context.reason {
                PluginStopReason::Replacement => 1,
                PluginStopReason::SessionRebind => 2,
                PluginStopReason::Shutdown => 3,
            },
            _ => 0,
        }
    }

    fn token(&self) -> u64 {
        match self {
            Self::Notification(notification) => notification.token,
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => {
                context.session_id.unwrap_or(0)
            }
            Self::Lifecycle(PluginLifecycleEvent::Stop(context)) => {
                context.next_generation.unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn lifecycle_data(&self) -> &[crate::lifecycle::PluginLifecycleEntry] {
        match self {
            Self::Lifecycle(PluginLifecycleEvent::Start(context)) => &context.data,
            _ => &[],
        }
    }
}

struct Preview1Store {
    wasi: WasiP1Ctx,
    host: Preview1Host,
    limits: GenerationStoreLimits,
}

struct Preview1Host {
    actor: bool,
    event: Option<CompatEvent>,
    completion_io: Option<CompletionIo>,
    coordination: CoordinationHub,
    filesystem_watches: FilesystemWatchHub,
    runtime_io: RuntimeIoHub,
    runtime_children: Arc<Mutex<HashMap<u64, [u64; 4]>>>,
    commands: mpsc::Sender<Command>,
    next_worker: Arc<AtomicU64>,
    worker_input_capacity: usize,
    worker_input_backlog: usize,
    worker_inputs: WorkerInputRegistry,
    stats: Arc<Preview1CompatStatsInner>,
}

struct WorkerStdin {
    inner: AsyncStdinStream,
    stats: Arc<Preview1CompatStatsInner>,
    entered: AtomicBool,
}

impl WorkerStdin {
    const fn observed(inner: AsyncStdinStream, stats: Arc<Preview1CompatStatsInner>) -> Self {
        Self {
            inner,
            stats,
            entered: AtomicBool::new(false),
        }
    }

    fn mark_entered(&self) {
        if !self.entered.swap(true, Ordering::AcqRel) {
            self.stats
                .worker_reads_started
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl IsTerminal for WorkerStdin {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdinStream for WorkerStdin {
    fn async_stream(&self) -> Box<dyn AsyncRead + Send + Sync> {
        self.mark_entered();
        self.inner.async_stream()
    }

    fn p2_stream(&self) -> Box<dyn wasmtime_wasi::p2::InputStream> {
        self.mark_entered();
        self.inner.p2_stream()
    }
}

impl host_abi::brood_preview1::BroodPreview1 for Preview1Host {
    fn event_kind(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u32, host_abi::types::Errno> {
        Ok(self.actor_event()?.kind())
    }

    fn event_sender_len(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u32, host_abi::types::Errno> {
        u32::try_from(self.actor_event()?.sender().len())
            .map_err(|_overflow| host_abi::types::Errno::Limit)
    }

    fn event_payload_len(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u32, host_abi::types::Errno> {
        u32::try_from(self.actor_event()?.payload().len())
            .map_err(|_overflow| host_abi::types::Errno::Limit)
    }

    fn event_operation(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u64, host_abi::types::Errno> {
        Ok(self.actor_event()?.operation())
    }

    fn event_value(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u64, host_abi::types::Errno> {
        Ok(self.actor_event()?.value())
    }

    fn event_token(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u64, host_abi::types::Errno> {
        Ok(self.actor_event()?.token())
    }

    fn event_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        sender: wiggle::GuestPtr<u8>,
        sender_len: u32,
        payload: wiggle::GuestPtr<u8>,
        payload_len: u32,
    ) -> Result<(), host_abi::types::Errno> {
        let event = self.actor_event()?;
        copy_to_guest(memory, sender, sender_len, event.sender())?;
        copy_to_guest(memory, payload, payload_len, event.payload())
    }

    fn event_lifecycle_data_count(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u32, host_abi::types::Errno> {
        u32::try_from(self.actor_event()?.lifecycle_data().len())
            .map_err(|_overflow| host_abi::types::Errno::Limit)
    }

    fn event_lifecycle_key_len(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        index: u32,
    ) -> Result<u32, host_abi::types::Errno> {
        self.actor_event()?
            .lifecycle_data()
            .get(index as usize)
            .ok_or(host_abi::types::Errno::Invalid)
            .and_then(|entry| {
                u32::try_from(entry.key.len()).map_err(|_overflow| host_abi::types::Errno::Limit)
            })
    }

    fn event_lifecycle_value_len(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        index: u32,
    ) -> Result<u32, host_abi::types::Errno> {
        self.actor_event()?
            .lifecycle_data()
            .get(index as usize)
            .ok_or(host_abi::types::Errno::Invalid)
            .and_then(|entry| {
                u32::try_from(entry.value.len()).map_err(|_overflow| host_abi::types::Errno::Limit)
            })
    }

    fn event_lifecycle_data_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        index: u32,
        key: wiggle::GuestPtr<u8>,
        key_len: u32,
        value: wiggle::GuestPtr<u8>,
        value_len: u32,
    ) -> Result<(), host_abi::types::Errno> {
        let entry = self
            .actor_event()?
            .lifecycle_data()
            .get(index as usize)
            .ok_or(host_abi::types::Errno::Invalid)?;
        copy_to_guest(memory, key, key_len, entry.key.as_bytes())?;
        copy_to_guest(memory, value, value_len, entry.value.as_bytes())
    }

    fn lifecycle_complete(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        status: u32,
        message: wiggle::GuestPtr<u8>,
        message_len: u32,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_actor()?;
        let message = read_guest_string(memory, message, message_len)?;
        let result = match status {
            0 => Ok(()),
            1 => Err(message),
            _ => return Err(host_abi::types::Errno::Invalid),
        };
        self.commands
            .try_send(Command::GuestLifecycleComplete(result))
            .map_err(map_command_error)
    }

    fn subprocess_spawn(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        command: wiggle::GuestPtr<u8>,
        command_len: u32,
        arguments: wiggle::GuestPtr<u8>,
        arguments_len: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        let command = read_guest_string(memory, command, command_len)?;
        let arguments = read_guest_arguments(memory, arguments, arguments_len)?;
        let process = self
            .runtime_io
            .spawn_process(&command, &arguments)
            .map_err(map_runtime_io_error)?;
        let child = process.child.raw();
        self.runtime_children
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .insert(
                child,
                [
                    process.stdin.raw(),
                    process.stdout.raw(),
                    process.stderr.raw(),
                    0,
                ],
            );
        Ok(child)
    }

    fn pty_spawn(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        command: wiggle::GuestPtr<u8>,
        command_len: u32,
        arguments: wiggle::GuestPtr<u8>,
        arguments_len: u32,
        rows: u32,
        cols: u32,
        terminal_profile: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        let command = read_guest_string(memory, command, command_len)?;
        let arguments = read_guest_arguments(memory, arguments, arguments_len)?;
        let rows = u16::try_from(rows).map_err(|_overflow| host_abi::types::Errno::Invalid)?;
        let cols = u16::try_from(cols).map_err(|_overflow| host_abi::types::Errno::Invalid)?;
        let terminal_profile = match terminal_profile {
            0 => TerminalProfile::Xterm256Color,
            _ => return Err(host_abi::types::Errno::Invalid),
        };
        let pty = self
            .runtime_io
            .spawn_pty(
                &command,
                &arguments,
                PtySpawnOptions {
                    rows,
                    cols,
                    terminal_profile,
                },
            )
            .map_err(map_runtime_io_error)?;
        let child = pty.child.raw();
        self.runtime_children
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .insert(child, [0, 0, 0, pty.terminal.raw()]);
        Ok(child)
    }

    fn pty_resize(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
        rows: u32,
        cols: u32,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_worker()?;
        let rows = u16::try_from(rows).map_err(|_overflow| host_abi::types::Errno::Invalid)?;
        let cols = u16::try_from(cols).map_err(|_overflow| host_abi::types::Errno::Invalid)?;
        self.runtime_io
            .resize_pty(child, rows, cols)
            .map_err(map_runtime_io_error)
    }

    async fn worker_loopback_connect(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        address: wiggle::GuestPtr<u8>,
        address_len: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        let address = read_guest_string(memory, address, address_len)?
            .parse::<SocketAddr>()
            .map_err(|_error| host_abi::types::Errno::Invalid)?;
        self.runtime_io
            .connect_loopback(address)
            .await
            .map(|stream| stream.raw())
            .map_err(map_runtime_io_error)
    }

    fn child_stream(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
        kind: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        let index = usize::try_from(kind).map_err(|_overflow| host_abi::types::Errno::Invalid)?;
        let raw = self
            .runtime_children
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .get(&child)
            .and_then(|streams| streams.get(index))
            .copied()
            .filter(|raw| *raw != 0)
            .ok_or(host_abi::types::Errno::Invalid)?;
        self.runtime_io.stream(raw).map_err(map_runtime_io_error)?;
        Ok(raw)
    }

    fn child_notification(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.runtime_io
            .child(child)
            .map(|child| child.notification().raw())
            .map_err(map_runtime_io_error)
    }

    fn child_status(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.runtime_io
            .child(child)
            .and_then(|child| child.status())
            .map(|status| match status {
                ChildStatus::Running => 0,
                ChildStatus::Exited(Some(_)) => 1,
                ChildStatus::Exited(None) => 2,
            })
            .map_err(map_runtime_io_error)
    }

    fn child_exit_code(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        match self
            .runtime_io
            .child(child)
            .and_then(|child| child.status())
            .map_err(map_runtime_io_error)?
        {
            ChildStatus::Exited(Some(code)) => {
                u32::try_from(code).map_err(|_negative| host_abi::types::Errno::Invalid)
            }
            ChildStatus::Running | ChildStatus::Exited(None) => {
                Err(host_abi::types::Errno::Unavailable)
            }
        }
    }

    fn child_resolved_command_len(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        let child = self.runtime_io.child(child).map_err(map_runtime_io_error)?;
        u32::try_from(
            child
                .resolved_command()
                .as_os_str()
                .as_encoded_bytes()
                .len(),
        )
        .map_err(|_overflow| host_abi::types::Errno::Limit)
    }

    fn child_resolved_command_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
        path: wiggle::GuestPtr<u8>,
        path_len: u32,
    ) -> Result<(), host_abi::types::Errno> {
        let child = self.runtime_io.child(child).map_err(map_runtime_io_error)?;
        copy_to_guest(
            memory,
            path,
            path_len,
            child.resolved_command().as_os_str().as_encoded_bytes(),
        )
    }

    async fn child_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        child: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.runtime_children
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .remove(&child);
        self.runtime_io
            .drop_child(child)
            .await
            .map_err(map_runtime_io_error)
    }

    fn stream_notification(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        stream: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.runtime_io
            .stream(stream)
            .map(|stream| stream.notification().raw())
            .map_err(map_runtime_io_error)
    }

    fn stream_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        stream: u64,
        bytes: wiggle::GuestPtr<u8>,
        bytes_len: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        match self
            .runtime_io
            .stream(stream)
            .and_then(|stream| stream.try_read(bytes_len as usize))
            .map_err(map_runtime_io_error)?
        {
            StreamRead::Data(data) => {
                copy_to_guest(memory, bytes, bytes_len, &data)?;
                u64::try_from(data.len()).map_err(|_overflow| host_abi::types::Errno::Limit)
            }
            StreamRead::WouldBlock => Ok(u64::MAX - 1),
            StreamRead::Eof => Ok(u64::MAX),
        }
    }

    fn stream_write(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        stream: u64,
        bytes: wiggle::GuestPtr<u8>,
        bytes_len: u32,
    ) -> Result<u32, host_abi::types::Errno> {
        let bytes = read_guest_bytes(memory, bytes, bytes_len)?;
        self.runtime_io
            .stream(stream)
            .and_then(|stream| stream.try_write(&bytes))
            .and_then(|count| {
                u32::try_from(count).map_err(|_overflow| RuntimeIoError::InvalidArgument)
            })
            .map_err(map_runtime_io_error)
    }

    fn stream_close_write(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        stream: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.runtime_io
            .stream(stream)
            .and_then(|stream| stream.close_write())
            .map_err(map_runtime_io_error)
    }

    async fn stream_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        stream: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.runtime_io
            .drop_stream(stream)
            .await
            .map_err(map_runtime_io_error)
    }

    fn timer_create(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        delay_ms: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.runtime_io
            .create_timer(Duration::from_millis(delay_ms))
            .map(|timer| timer.raw())
            .map_err(map_runtime_io_error)
    }

    fn timer_notification(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        timer: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.runtime_io
            .timer(timer)
            .map(|timer| timer.notification().raw())
            .map_err(map_runtime_io_error)
    }

    fn timer_status(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        timer: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.runtime_io
            .timer(timer)
            .and_then(|timer| timer.status())
            .map(|status| u32::from(matches!(status, TimerStatus::Expired)))
            .map_err(map_runtime_io_error)
    }

    async fn timer_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        timer: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.runtime_io
            .drop_timer(timer)
            .await
            .map_err(map_runtime_io_error)
    }

    fn completion_submit_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        path: wiggle::GuestPtr<u8>,
        path_len: u32,
        offset: u64,
        max_bytes: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        let path = read_guest_string(memory, path, path_len)?;
        self.actor_completion_io()?
            .submit_read(path, offset, max_bytes as usize)
            .map(OperationId::sequence)
            .map_err(map_completion_error)
    }

    fn completion_submit_write(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        path: wiggle::GuestPtr<u8>,
        path_len: u32,
        offset: u64,
        bytes: wiggle::GuestPtr<u8>,
        bytes_len: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        let path = read_guest_string(memory, path, path_len)?;
        let bytes = read_guest_bytes(memory, bytes, bytes_len)?;
        self.actor_completion_io()?
            .submit_write(path, offset, bytes)
            .map(OperationId::sequence)
            .map_err(map_completion_error)
    }

    fn completion_cancel(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        operation: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        let io = self.actor_completion_io()?;
        Ok(u32::from(
            io.cancel(OperationId::new(io.generation(), operation)),
        ))
    }

    fn filesystem_watch_create(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        path: wiggle::GuestPtr<u8>,
        path_len: u32,
        recursive: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_actor()?;
        let path = read_guest_string(memory, path, path_len)?;
        let recursive = match recursive {
            0 => false,
            1 => true,
            _ => return Err(host_abi::types::Errno::Invalid),
        };
        self.filesystem_watches
            .create(Path::new(&path), recursive)
            .map(|(watch, _notification)| watch.raw())
            .map_err(map_filesystem_watch_error)
    }

    fn filesystem_watch_notification(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        watch: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.filesystem_watches
            .notification(FilesystemWatchId::from_raw(watch))
            .map(NotificationId::raw)
            .map_err(map_filesystem_watch_error)
    }

    fn filesystem_watch_event_bytes(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        watch: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.filesystem_watches
            .pending_bytes(FilesystemWatchId::from_raw(watch))
            .and_then(|bytes| u32::try_from(bytes).map_err(|_overflow| FilesystemWatchError::Limit))
            .map_err(map_filesystem_watch_error)
    }

    fn filesystem_watch_read(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        watch: u64,
        events: wiggle::GuestPtr<u8>,
        events_len: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        let (generation, bytes) = self
            .filesystem_watches
            .read(FilesystemWatchId::from_raw(watch), events_len as usize)
            .map_err(map_filesystem_watch_error)?;
        copy_to_guest(memory, events, events_len, &bytes)?;
        Ok(generation)
    }

    fn filesystem_watch_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        watch: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_actor()?;
        self.filesystem_watches
            .drop_watch(FilesystemWatchId::from_raw(watch))
            .map_err(map_filesystem_watch_error)
    }

    fn worker_spawn(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        argument: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_actor()?;
        let worker = allocate_worker_id(&self.next_worker)?;
        self.worker_inputs
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .insert(worker, None);
        self.commands
            .try_send(Command::GuestSpawnWorker {
                worker,
                argument,
                stdin: None,
                input_pump: None,
            })
            .map_err(|error| {
                if let Ok(mut inputs) = self.worker_inputs.lock() {
                    inputs.remove(&worker);
                }
                map_command_error(error)
            })?;
        Ok(worker.raw())
    }

    fn worker_spawn_with_stdin(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        argument: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_actor()?;
        let worker = allocate_worker_id(&self.next_worker)?;
        let (input, stdin) = tokio::io::duplex(self.worker_input_capacity);
        let (input_sender, input_receiver) = mpsc::channel(self.worker_input_backlog);
        self.worker_inputs
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .insert(worker, Some(input_sender));
        self.commands
            .try_send(Command::GuestSpawnWorker {
                worker,
                argument,
                stdin: Some(WorkerStdin::observed(
                    AsyncStdinStream::new(stdin),
                    Arc::clone(&self.stats),
                )),
                input_pump: Some((input, input_receiver)),
            })
            .map_err(|error| {
                if let Ok(mut inputs) = self.worker_inputs.lock() {
                    inputs.remove(&worker);
                }
                map_command_error(error)
            })?;
        Ok(worker.raw())
    }

    fn worker_cancel(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        worker: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.ensure_actor()?;
        let worker = Preview1WorkerId(worker);
        if !self
            .worker_inputs
            .lock()
            .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?
            .contains_key(&worker)
        {
            return Ok(0);
        }
        self.commands
            .try_send(Command::GuestCancelWorker(worker))
            .map_err(map_command_error)?;
        Ok(1)
    }

    fn worker_input(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        worker: u64,
        bytes: wiggle::GuestPtr<u8>,
        bytes_len: u32,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_actor()?;
        let bytes = read_guest_bytes(memory, bytes, bytes_len)?;
        if bytes.len() > self.worker_input_capacity {
            return Err(host_abi::types::Errno::Limit);
        }
        let input = {
            let inputs = self
                .worker_inputs
                .lock()
                .map_err(|_poisoned| host_abi::types::Errno::Unavailable)?;
            inputs
                .get(&Preview1WorkerId(worker))
                .and_then(Clone::clone)
                .ok_or(host_abi::types::Errno::Unavailable)?
        };
        input.try_send(bytes).map_err(map_command_error)
    }

    fn notification_create(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_actor()?;
        self.coordination
            .create_notification()
            .map(NotificationId::raw)
            .map_err(map_notification_error)
    }

    fn notification_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        notification: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_actor()?;
        self.coordination
            .drop_notification(NotificationId::from_raw(notification))
            .map_err(map_notification_error)
    }

    fn notification_signal(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        notification: u64,
        generation: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.coordination
            .signal(NotificationId::from_raw(notification), generation)
            .map_err(map_notification_error)
    }

    fn notification_watch(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        notification: u64,
        observed_generation: u64,
        token: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.ensure_actor()?;
        self.coordination
            .watch_actor(
                NotificationId::from_raw(notification),
                observed_generation,
                token,
            )
            .map(|result| u32::from(matches!(result, ActorWatchResult::Queued { .. })))
            .map_err(map_notification_error)
    }

    fn notification_unwatch(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        notification: u64,
    ) -> Result<u32, host_abi::types::Errno> {
        self.ensure_actor()?;
        self.coordination
            .unwatch_actor(NotificationId::from_raw(notification))
            .map(u32::from)
            .map_err(map_notification_error)
    }

    fn notification_pollable(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        notification: u64,
        observed_generation: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        self.coordination
            .notification_pollable(NotificationId::from_raw(notification), observed_generation)
            .map(PollableId::raw)
            .map_err(map_poll_error)
    }

    fn pollable_drop(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        pollable: u64,
    ) -> Result<(), host_abi::types::Errno> {
        self.ensure_worker()?;
        self.coordination
            .drop_pollable(PollableId::from_raw(pollable))
            .map_err(map_poll_error)
    }

    fn pollable_generation(
        &mut self,
        _memory: &mut wiggle::GuestMemory<'_>,
        pollable: u64,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        self.coordination
            .try_poll(&[PollableId::from_raw(pollable)])
            .map_err(map_poll_error)?
            .first()
            .map(|ready| ready.generation)
            .ok_or(host_abi::types::Errno::Unavailable)
    }

    async fn worker_wait_any(
        &mut self,
        memory: &mut wiggle::GuestMemory<'_>,
        pollables: wiggle::GuestPtr<[u64]>,
        timeout_ms: u64,
        mode: u32,
    ) -> Result<u64, host_abi::types::Errno> {
        self.ensure_worker()?;
        let pollables = memory
            .to_vec(pollables)
            .map_err(|_guest| host_abi::types::Errno::Invalid)?
            .into_iter()
            .map(PollableId::from_raw)
            .collect::<Vec<_>>();
        let timeout = (timeout_ms != u64::MAX).then(|| Duration::from_millis(timeout_ms));
        let mode = match mode {
            0 => WaitMode::Biased,
            1 => WaitMode::Fair,
            _ => return Err(host_abi::types::Errno::Invalid),
        };
        self.coordination
            .wait_any(&pollables, timeout, mode)
            .await
            .map(|ready| ready.pollable.raw())
            .map_err(map_poll_error)
    }
}

impl Preview1Host {
    const fn ensure_actor(&self) -> Result<(), host_abi::types::Errno> {
        if self.actor {
            Ok(())
        } else {
            Err(host_abi::types::Errno::Unavailable)
        }
    }

    const fn ensure_worker(&self) -> Result<(), host_abi::types::Errno> {
        if self.actor {
            Err(host_abi::types::Errno::Unavailable)
        } else {
            Ok(())
        }
    }

    fn actor_event(&self) -> Result<&CompatEvent, host_abi::types::Errno> {
        self.ensure_actor()?;
        self.event
            .as_ref()
            .ok_or(host_abi::types::Errno::Unavailable)
    }

    fn actor_completion_io(&self) -> Result<&CompletionIo, host_abi::types::Errno> {
        self.completion_io
            .as_ref()
            .ok_or(host_abi::types::Errno::Unavailable)
    }
}

fn copy_to_guest(
    memory: &mut wiggle::GuestMemory<'_>,
    pointer: wiggle::GuestPtr<u8>,
    capacity: u32,
    bytes: &[u8],
) -> Result<(), host_abi::types::Errno> {
    let len = u32::try_from(bytes.len()).map_err(|_overflow| host_abi::types::Errno::Limit)?;
    if capacity < len {
        return Err(host_abi::types::Errno::Limit);
    }
    memory
        .copy_from_slice(bytes, pointer.as_array(len))
        .map_err(|_guest| host_abi::types::Errno::Invalid)
}

fn read_guest_bytes(
    memory: &wiggle::GuestMemory<'_>,
    pointer: wiggle::GuestPtr<u8>,
    len: u32,
) -> Result<Vec<u8>, host_abi::types::Errno> {
    memory
        .to_vec(pointer.as_array(len))
        .map_err(|_guest| host_abi::types::Errno::Invalid)
}

fn read_guest_string(
    memory: &wiggle::GuestMemory<'_>,
    pointer: wiggle::GuestPtr<u8>,
    len: u32,
) -> Result<String, host_abi::types::Errno> {
    String::from_utf8(read_guest_bytes(memory, pointer, len)?)
        .map_err(|_utf8| host_abi::types::Errno::Invalid)
}

fn read_guest_arguments(
    memory: &wiggle::GuestMemory<'_>,
    arguments: wiggle::GuestPtr<u8>,
    arguments_len: u32,
) -> Result<Vec<String>, host_abi::types::Errno> {
    let bytes = read_guest_bytes(memory, arguments, arguments_len)?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    bytes
        .split(|byte| *byte == 0)
        .map(|argument| {
            std::str::from_utf8(argument)
                .map(str::to_string)
                .map_err(|_utf8| host_abi::types::Errno::Invalid)
        })
        .collect()
}

const fn map_runtime_io_error(error: RuntimeIoError) -> host_abi::types::Errno {
    match error {
        RuntimeIoError::StaleHandle | RuntimeIoError::Closed | RuntimeIoError::HostFailure => {
            host_abi::types::Errno::Unavailable
        }
        RuntimeIoError::LimitReached | RuntimeIoError::WouldBlock => host_abi::types::Errno::Limit,
        RuntimeIoError::NotPermitted | RuntimeIoError::InvalidArgument => {
            host_abi::types::Errno::Invalid
        }
    }
}

const fn map_completion_error(error: CompletionIoError) -> host_abi::types::Errno {
    match error {
        CompletionIoError::InvalidArgument => host_abi::types::Errno::Invalid,
        CompletionIoError::LimitReached | CompletionIoError::BacklogFull => {
            host_abi::types::Errno::Limit
        }
        CompletionIoError::ActorStopped | CompletionIoError::NotPermitted => {
            host_abi::types::Errno::Unavailable
        }
    }
}

const fn map_notification_error(error: NotificationError) -> host_abi::types::Errno {
    match error {
        NotificationError::InvalidHandle | NotificationError::InvalidGeneration => {
            host_abi::types::Errno::Invalid
        }
        NotificationError::LimitReached
        | NotificationError::BacklogFull
        | NotificationError::AlreadyWatched => host_abi::types::Errno::Limit,
        NotificationError::Stopped => host_abi::types::Errno::Unavailable,
    }
}

const fn map_poll_error(error: PollError) -> host_abi::types::Errno {
    match error {
        PollError::InvalidPollable | PollError::DuplicatePollable | PollError::EmptyWaitSet => {
            host_abi::types::Errno::Invalid
        }
        PollError::LimitReached => host_abi::types::Errno::Limit,
        PollError::Stopped | PollError::TimedOut | PollError::Cancelled => {
            host_abi::types::Errno::Unavailable
        }
    }
}

fn map_filesystem_watch_error(error: FilesystemWatchError) -> host_abi::types::Errno {
    match error {
        FilesystemWatchError::Invalid => host_abi::types::Errno::Invalid,
        FilesystemWatchError::Limit => host_abi::types::Errno::Limit,
        FilesystemWatchError::Stopped
        | FilesystemWatchError::NotPermitted
        | FilesystemWatchError::NotReady
        | FilesystemWatchError::Backend(_)
        | FilesystemWatchError::Unavailable => host_abi::types::Errno::Unavailable,
    }
}

fn map_command_error<T>(error: mpsc::error::TrySendError<T>) -> host_abi::types::Errno {
    match error {
        mpsc::error::TrySendError::Full(_) => host_abi::types::Errno::Limit,
        mpsc::error::TrySendError::Closed(_) => host_abi::types::Errno::Unavailable,
    }
}

fn allocate_worker_id(next_worker: &AtomicU64) -> Result<Preview1WorkerId, host_abi::types::Errno> {
    next_worker
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map(Preview1WorkerId)
        .map_err(|_overflow| host_abi::types::Errno::Limit)
}

fn remove_worker_input(inputs: &WorkerInputRegistry, worker: Preview1WorkerId) {
    if let Ok(mut inputs) = inputs.lock() {
        inputs.remove(&worker);
    }
}

fn remove_queued_worker_failure(
    operations: &mut VecDeque<ActorOperation>,
    worker: Preview1WorkerId,
) -> bool {
    let previous_len = operations.len();
    operations.retain(|operation| {
        !matches!(
            operation,
            ActorOperation::Dispatch(CompatEvent::WorkerFailed {
                worker: failed_worker,
                ..
            }) if *failed_worker == worker
        )
    });
    operations.len() != previous_len
}

/// Stable identifier for a worker scoped to one runtime (and therefore actor).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Preview1WorkerId(u64);

impl Preview1WorkerId {
    /// Return the runtime-local numeric identifier.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Default)]
struct Preview1CompatStatsInner {
    mailbox_queued: AtomicU64,
    mailbox_dropped: AtomicU64,
    mailbox_dispatched: AtomicU64,
    completion_delivered: AtomicU64,
    notifications_delivered: AtomicU64,
    workers_started: AtomicU64,
    workers_active: AtomicUsize,
    worker_reads_started: AtomicU64,
    workers_finished: AtomicU64,
    workers_failed: AtomicU64,
    workers_cancelled: AtomicU64,
    last_worker_value: AtomicU64,
}

fn increment_saturating(counter: &AtomicU64) {
    let _previous = counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(current.saturating_add(1))
        })
        .unwrap_or(u64::MAX);
}

/// Observable lifecycle counters for daemon status and nested tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preview1CompatStats {
    /// Actor generation; changes on every reload.
    pub generation: u64,
    /// Mailbox messages accepted by the bounded runtime command queue.
    pub mailbox_queued: u64,
    /// Mailbox messages rejected before admission.
    pub mailbox_dropped: u64,
    /// Configured global mailbox backlog.
    pub mailbox_capacity: usize,
    /// Mailbox events whose guest dispatcher returned successfully.
    pub mailbox_dispatched: u64,
    /// Typed completion events delivered to the guest dispatcher.
    pub completion_delivered: u64,
    /// Coalesced notification events delivered to the actor dispatcher.
    pub notifications_delivered: u64,
    /// Live notification resources in this generation.
    pub notifications: usize,
    /// Live pollables in this generation.
    pub pollables: usize,
    /// Pollable registrations currently suspended in worker waits.
    pub wait_registrations: usize,
    /// Coalesced actor notification IDs waiting in the reserved lane.
    pub pending_notification_events: usize,
    /// Live logical filesystem watches owned by this generation.
    pub filesystem_watches: usize,
    /// Configured logical-watch ceiling.
    pub max_filesystem_watches: usize,
    /// Configured per-watch queued-event ceiling.
    pub max_filesystem_watch_events: usize,
    /// Capacity/backend overflows collapsed to rescan markers.
    pub filesystem_watch_overflows: u64,
    /// All rescan markers, including initial native setup.
    pub filesystem_watch_rescans: u64,
    /// Workers admitted after Store/export preparation and scheduled to run.
    pub workers_started: u64,
    /// Workers whose guest calls are currently live.
    pub workers_active: usize,
    /// Worker standard-WASI stdin streams first requested by guest execution.
    pub worker_reads_started: u64,
    /// Workers which returned normally.
    pub workers_finished: u64,
    /// Workers which failed before or during execution.
    pub workers_failed: u64,
    /// Workers whose Store was dropped after cancellation.
    pub workers_cancelled: u64,
    /// Return value from the latest successfully finished guest-spawned worker.
    pub last_worker_value: u64,
}

/// Error returned by the Preview 1 compatibility lane.
#[derive(Debug, thiserror::Error)]
pub enum Preview1WorkerError {
    /// Runtime configuration is invalid.
    #[error("invalid Preview 1 worker configuration: {0}")]
    InvalidConfig(&'static str),
    /// An actor-owned WASI capability could not be installed.
    #[error("invalid Preview 1 worker capability: {0}")]
    InvalidCapability(String),
    /// The guest is not a compatible `wasm32-wasip1-threads` module.
    #[error("invalid Preview 1 worker module: {0}")]
    InvalidModule(String),
    /// A guest export trapped or did not satisfy its ABI.
    #[error("Preview 1 worker guest failure: {0}")]
    Guest(String),
    /// A configured bound has been reached.
    #[error("Preview 1 worker capacity reached: {0}")]
    AtCapacity(&'static str),
    /// The worker was cancelled by its actor.
    #[error("Preview 1 worker was cancelled")]
    Cancelled,
    /// The actor runtime has stopped.
    #[error("Preview 1 worker runtime has stopped")]
    RuntimeStopped,
    /// The parent Tokio task failed.
    #[error("Preview 1 worker parent task failed: {0}")]
    ParentTask(String),
}

/// Join handle for one actor-scoped worker.
pub struct Preview1Worker {
    id: Preview1WorkerId,
    completion: oneshot::Receiver<Result<u32, Preview1WorkerError>>,
}

impl Preview1Worker {
    /// Return this worker's actor-local identifier.
    pub const fn id(&self) -> Preview1WorkerId {
        self.id
    }

    /// Wait for the guest worker export to return.
    pub async fn join(self) -> Result<u32, Preview1WorkerError> {
        self.completion
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?
    }
}

struct Preview1CompletionSupervisor {
    io: CompletionIo,
    task: Option<TokioJoinHandle<()>>,
    abort: TokioAbortHandle,
    joined: bool,
}

impl Preview1CompletionSupervisor {
    fn new(io: CompletionIo, task: TokioJoinHandle<()>) -> Self {
        let abort = task.abort_handle();
        Self {
            io,
            task: Some(task),
            abort,
            joined: false,
        }
    }

    const fn io(&self) -> &CompletionIo {
        &self.io
    }

    async fn shutdown(&mut self) {
        self.io.stop();
        if let Some(task) = self.task.take() {
            let _result = task.await;
        }
        self.joined = true;
    }
}

impl Drop for Preview1CompletionSupervisor {
    fn drop(&mut self) {
        self.io.stop();
        if !self.joined {
            self.abort.abort();
        }
    }
}

/// One actor and its scoped Preview 1 compatibility workers.
pub struct Preview1WorkerRuntime {
    commands: mpsc::Sender<Command>,
    mailbox_slots: Arc<Semaphore>,
    mailbox_capacity: usize,
    parent: Option<TokioJoinHandle<Result<(), Preview1WorkerError>>>,
    generation: u64,
    stats: Arc<Preview1CompatStatsInner>,
    coordination: CoordinationHub,
    filesystem_watches: FilesystemWatchHub,
    runtime_io: RuntimeIoHub,
    terminal_error: Arc<Mutex<Option<String>>>,
    limits: WasmResourceLimits,
    byte_budget: GenerationByteBudget,
}

impl Preview1WorkerRuntime {
    /// Load a `wasm32-wasip1-threads` core module and start its actor owner.
    ///
    /// Each worker gets a fresh store-local WASI table derived from the same
    /// immutable `capabilities`; a worker cannot gain authority over its actor.
    pub async fn load(
        path: impl AsRef<Path>,
        capabilities: Preview1ActorCapabilities,
        config: Preview1WorkerConfig,
    ) -> Result<Self, Preview1WorkerError> {
        Self::load_with_watcher_service(
            path,
            capabilities,
            config,
            NativeFilesystemWatcherService::new(),
        )
        .await
    }

    /// Load using a daemon-owned native watcher service shared by generations.
    pub async fn load_with_watcher_service(
        path: impl AsRef<Path>,
        capabilities: Preview1ActorCapabilities,
        config: Preview1WorkerConfig,
        watcher_service: NativeFilesystemWatcherService,
    ) -> Result<Self, Preview1WorkerError> {
        Self::load_with_completion_observer(
            path.as_ref(),
            capabilities,
            config,
            watcher_service,
            |_io| {},
        )
        .await
    }

    async fn load_with_completion_observer<F>(
        path: &Path,
        capabilities: Preview1ActorCapabilities,
        config: Preview1WorkerConfig,
        watcher_service: NativeFilesystemWatcherService,
        observe_completion: F,
    ) -> Result<Self, Preview1WorkerError>
    where
        F: FnOnce(&CompletionIo) + Send,
    {
        validate_config(config)?;

        let mut engine_config = Config::new();
        engine_config.wasm_threads(true).shared_memory(true);
        engine_config.epoch_interruption(true);
        let engine = Engine::new(&engine_config)
            .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
        let module = Module::from_file(&engine, path)
            .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
        let memory_type = module
            .imports()
            .find_map(|import| match import.ty() {
                ExternType::Memory(memory)
                    if import.module() == SHARED_MEMORY_IMPORT_MODULE
                        && import.name() == SHARED_MEMORY_IMPORT_NAME =>
                {
                    Some(memory)
                }
                _ => None,
            })
            .ok_or_else(|| {
                Preview1WorkerError::InvalidModule(format!(
                    "missing {SHARED_MEMORY_IMPORT_MODULE}::{SHARED_MEMORY_IMPORT_NAME} memory import"
                ))
            })?;
        if !memory_type.is_shared() {
            return Err(Preview1WorkerError::InvalidModule(
                "env::memory must be shared".to_string(),
            ));
        }
        let maximum_pages = memory_type.maximum().ok_or_else(|| {
            Preview1WorkerError::InvalidModule(
                "env::memory must declare a maximum for generation memory containment".to_string(),
            )
        })?;
        let maximum_bytes = usize::try_from(maximum_pages)
            .ok()
            .and_then(|pages| pages.checked_mul(64 * 1024))
            .ok_or_else(|| {
                Preview1WorkerError::InvalidModule(
                    "env::memory maximum overflows host byte accounting".to_string(),
                )
            })?;
        if maximum_bytes > config.limits.generation_memory_bytes {
            return Err(Preview1WorkerError::InvalidModule(format!(
                "env::memory maximum {maximum_bytes} exceeds {} byte generation limit",
                config.limits.generation_memory_bytes
            )));
        }
        let memory = SharedMemory::new(&engine, memory_type)
            .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;

        let generation = NEXT_ACTOR_GENERATION
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != 0).then(|| current.checked_add(1)).flatten()
            })
            .map_err(|_exhausted| Preview1WorkerError::AtCapacity("actor generations"))?;
        let (coordination, notification_rx) = CoordinationHub::new(
            generation,
            CoordinationConfig {
                max_notifications: config.max_notifications,
                max_pollables: config.max_pollables,
                max_wait_set: config.max_wait_set,
            },
        )
        .map_err(|error| {
            Preview1WorkerError::InvalidConfig(match error {
                NotificationError::InvalidGeneration => "invalid coordination configuration",
                _ => "coordination resources could not be initialized",
            })
        })?;
        let completion_config = CompletionIoConfig {
            filesystem: capabilities
                .filesystem
                .as_ref()
                .map(|grant| FilesystemGrant {
                    host_path: grant.host_path.clone(),
                    guest_path: grant.guest_path.clone(),
                    writable: grant.writable,
                }),
            max_outstanding: config.max_outstanding_io,
            max_completion_bytes: config.max_completion_bytes,
        };
        let filesystem_watches = FilesystemWatchHub::new_with_service(
            watcher_service,
            completion_config.filesystem.as_ref(),
            coordination.clone(),
            config.max_filesystem_watches,
            config.max_filesystem_watch_events,
        )
        .map_err(|error| Preview1WorkerError::InvalidCapability(error.to_string()))?;
        let runtime_io = RuntimeIoHub::new(
            generation,
            RuntimeIoConfig {
                capabilities: capabilities.runtime_io.clone(),
                ..RuntimeIoConfig::default()
            },
            coordination.clone(),
        )
        .map_err(|error| Preview1WorkerError::InvalidCapability(error.to_string()))?;
        let (completion_io, completion_rx, completion_task) =
            start_completion_io(generation, &completion_config)
                .map_err(|error| Preview1WorkerError::InvalidCapability(error.to_string()))?;
        let mut completion_supervisor =
            Preview1CompletionSupervisor::new(completion_io, completion_task);
        observe_completion(completion_supervisor.io());
        let (commands, receiver) = mpsc::channel(config.command_capacity);
        let mailbox_slots = Arc::new(Semaphore::new(config.mailbox_backlog));
        let mailbox_capacity = config.mailbox_backlog;
        let byte_budget = GenerationByteBudget::new(config.limits.generation_host_bytes);
        let stats = Arc::new(Preview1CompatStatsInner::default());
        let next_worker = Arc::new(AtomicU64::new(1));
        let worker_inputs = Arc::new(Mutex::new(HashMap::new()));
        let runtime_children = Arc::new(Mutex::new(HashMap::new()));
        let initialized: Result<_, Preview1WorkerError> = async {
            let epoch_ticker = EpochTicker::start(engine.clone(), config.epoch_tick_interval)?;
            // The dispatcher is completion-driven and receives no blocking WASI
            // filesystem or stdin surface. Only worker stores inherit the grant.
            let actor_wasi = build_wasi_context(&Preview1ActorCapabilities::default(), None)?;
            let mut actor_store = Store::new(
                &engine,
                Preview1Store {
                    wasi: actor_wasi,
                    host: Preview1Host {
                        actor: true,
                        event: None,
                        completion_io: Some(completion_supervisor.io().clone()),
                        coordination: coordination.clone(),
                        filesystem_watches: filesystem_watches.clone(),
                        runtime_io: runtime_io.clone(),
                        runtime_children: Arc::clone(&runtime_children),
                        commands: commands.clone(),
                        next_worker: Arc::clone(&next_worker),
                        worker_input_capacity: config.worker_input_capacity,
                        worker_input_backlog: config.worker_input_backlog,
                        worker_inputs: Arc::clone(&worker_inputs),
                        stats: Arc::clone(&stats),
                    },
                    limits: config.limits.store_limits(),
                },
            );
            actor_store.limiter(|data| &mut data.limits);
            configure_actor_yield(&mut actor_store, config.actor_yield_ticks);
            let actor_instance = instantiate(&engine, &module, &memory, &mut actor_store).await?;
            validate_actor_exports(&mut actor_store, actor_instance).await?;
            Ok((actor_store, actor_instance, epoch_ticker))
        }
        .await;
        let (actor_store, actor_instance, epoch_ticker) = match initialized {
            Ok(initialized) => initialized,
            Err(error) => {
                completion_supervisor.shutdown().await;
                filesystem_watches.stop();
                return Err(error);
            }
        };

        let terminal_error = Arc::new(Mutex::new(None));
        let parent_terminal_error = Arc::clone(&terminal_error);
        let parent_coordination = coordination.clone();
        let parent_runtime_io = runtime_io.clone();
        let parent_stats = Arc::clone(&stats);
        let parent = tokio::spawn(async move {
            let result = parent_loop(
                engine,
                module,
                memory,
                actor_store,
                actor_instance,
                capabilities,
                completion_rx,
                notification_rx,
                parent_coordination,
                parent_runtime_io,
                completion_supervisor,
                next_worker,
                worker_inputs,
                parent_stats,
                config,
                receiver,
                epoch_ticker,
            )
            .await;
            if let Err(error) = &result
                && let Ok(mut terminal_error) = parent_terminal_error.lock()
            {
                *terminal_error = Some(error.to_string());
            }
            result
        });
        Ok(Self {
            commands,
            mailbox_slots,
            mailbox_capacity,
            parent: Some(parent),
            generation,
            stats,
            coordination,
            filesystem_watches,
            runtime_io,
            terminal_error,
            limits: config.limits,
            byte_budget,
        })
    }

    /// Enqueue one normal mailbox message without blocking the daemon thread.
    pub fn try_send_message(
        &self,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<(), Preview1WorkerError> {
        if sender.len() > self.limits.mailbox_sender_bytes {
            increment_saturating(&self.stats.mailbox_dropped);
            return Err(Preview1WorkerError::AtCapacity("mailbox sender bytes"));
        }
        if payload.len() > self.limits.mailbox_body_bytes {
            increment_saturating(&self.stats.mailbox_dropped);
            return Err(Preview1WorkerError::AtCapacity("mailbox body bytes"));
        }
        let byte_permit = self
            .byte_budget
            .try_reserve(sender.len().saturating_add(payload.len()))
            .map_err(|_error| {
                increment_saturating(&self.stats.mailbox_dropped);
                Preview1WorkerError::AtCapacity("generation host bytes")
            })?;
        let permit = Arc::clone(&self.mailbox_slots)
            .try_acquire_owned()
            .map_err(|_no_permit| {
                increment_saturating(&self.stats.mailbox_dropped);
                Preview1WorkerError::AtCapacity("mailbox backlog")
            })?;
        if let Err(error) = self.commands.try_send(Command::Mailbox {
            sender,
            payload,
            permit,
            byte_permit,
        }) {
            increment_saturating(&self.stats.mailbox_dropped);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => {
                    Preview1WorkerError::AtCapacity("mailbox command queue")
                }
                mpsc::error::TrySendError::Closed(_) => Preview1WorkerError::RuntimeStopped,
            });
        }
        self.stats.mailbox_queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Snapshot daemon-observable lifecycle counters.
    pub fn stats(&self) -> Preview1CompatStats {
        let coordination = self.coordination.stats();
        let filesystem_watches = self.filesystem_watches.stats();
        Preview1CompatStats {
            generation: self.generation,
            mailbox_queued: self.stats.mailbox_queued.load(Ordering::Relaxed),
            mailbox_dropped: self.stats.mailbox_dropped.load(Ordering::Relaxed),
            mailbox_capacity: self.mailbox_capacity,
            mailbox_dispatched: self.stats.mailbox_dispatched.load(Ordering::Relaxed),
            completion_delivered: self.stats.completion_delivered.load(Ordering::Relaxed),
            notifications_delivered: self.stats.notifications_delivered.load(Ordering::Relaxed),
            notifications: coordination.notifications,
            pollables: coordination.pollables,
            wait_registrations: coordination.wait_registrations,
            pending_notification_events: coordination.pending_actor_events,
            filesystem_watches: filesystem_watches.watches,
            max_filesystem_watches: filesystem_watches.max_watches,
            max_filesystem_watch_events: filesystem_watches.max_events,
            filesystem_watch_overflows: filesystem_watches.overflows,
            filesystem_watch_rescans: filesystem_watches.rescans,
            workers_started: self.stats.workers_started.load(Ordering::Relaxed),
            workers_active: self.stats.workers_active.load(Ordering::Acquire),
            worker_reads_started: self.stats.worker_reads_started.load(Ordering::Relaxed),
            workers_finished: self.stats.workers_finished.load(Ordering::Relaxed),
            workers_failed: self.stats.workers_failed.load(Ordering::Relaxed),
            workers_cancelled: self.stats.workers_cancelled.load(Ordering::Relaxed),
            last_worker_value: self.stats.last_worker_value.load(Ordering::Relaxed),
        }
    }

    pub fn filesystem_watch_stats(&self) -> crate::filesystem_watch::FilesystemWatchStats {
        self.filesystem_watches.stats()
    }

    pub fn runtime_io_stats(&self) -> crate::runtime_io::RuntimeIoStats {
        self.runtime_io.stats()
    }

    fn try_take_terminal_error(&self) -> Option<String> {
        self.terminal_error.lock().ok()?.take()
    }

    /// Call a one-argument actor export.
    ///
    /// Actor exports are serialized. They must remain completion-driven and
    /// must not wait on worker synchronization primitives.
    pub async fn call_actor(
        &self,
        export: impl Into<String>,
        argument: u32,
    ) -> Result<u32, Preview1WorkerError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::ActorCall {
                export: export.into(),
                argument,
                reply,
            })
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?;
        response
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?
    }

    /// Spawn one worker using the actor's capabilities and an empty stdin.
    ///
    /// The export must have the core-Wasm signature `(i32) -> i32`. The
    /// returned handle becomes ready only after the actor has reserved a
    /// disjoint stack/TLS slab and the worker call has started.
    pub async fn spawn_worker(
        &self,
        export: impl Into<String>,
        argument: u32,
    ) -> Result<Preview1Worker, Preview1WorkerError> {
        self.spawn_worker_with_input(export.into(), argument, None)
            .await
    }

    /// Spawn one worker with an actor-provided asynchronous stdin stream.
    ///
    /// Filesystem and other ambient authority still come only from the actor's
    /// immutable capability grant. This stream is intended for actor-private
    /// channels which ordinary `read(0, ...)` can consume.
    pub async fn spawn_worker_with_stdin(
        &self,
        export: impl Into<String>,
        argument: u32,
        stdin: AsyncStdinStream,
    ) -> Result<Preview1Worker, Preview1WorkerError> {
        self.spawn_worker_with_input(export.into(), argument, Some(stdin))
            .await
    }

    async fn spawn_worker_with_input(
        &self,
        export: String,
        argument: u32,
        stdin: Option<AsyncStdinStream>,
    ) -> Result<Preview1Worker, Preview1WorkerError> {
        let stdin = stdin.map(|stdin| WorkerStdin::observed(stdin, Arc::clone(&self.stats)));
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::SpawnWorker {
                export,
                argument,
                stdin,
                reply,
            })
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?;
        response
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?
    }

    /// Cancel a worker without delivering a late successful result.
    ///
    /// Dropping a Wasmtime call future cancels asynchronous Preview 1 waits,
    /// but cannot preempt arbitrary guest CPU code or an operating-system
    /// operation which Wasmtime already delegated to a blocking thread.
    pub async fn cancel_worker(&self, worker: Preview1WorkerId) -> Result<(), Preview1WorkerError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::CancelWorker { worker, reply })
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?;
        response
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?
    }

    /// Stop the actor and all of its workers, then await parent-task teardown.
    pub async fn shutdown(&mut self) -> Result<(), Preview1WorkerError> {
        if self.parent.is_none() {
            return Ok(());
        }

        self.filesystem_watches.stop();
        self.runtime_io.teardown().await;
        self.coordination.stop();

        let (reply, response) = oneshot::channel();
        let sent = self
            .commands
            .send(Command::Shutdown { reply })
            .await
            .is_ok();
        if sent {
            response
                .await
                .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?;
        }
        self.await_parent().await
    }

    async fn await_parent(&mut self) -> Result<(), Preview1WorkerError> {
        let Some(parent) = self.parent.take() else {
            return Ok(());
        };
        parent
            .await
            .map_err(|error| Preview1WorkerError::ParentTask(error.to_string()))?
    }
}

impl Drop for Preview1WorkerRuntime {
    fn drop(&mut self) {
        self.filesystem_watches.stop();
        self.coordination.stop();
        if let Some(parent) = self.parent.take() {
            parent.abort();
        }
    }
}

/// Daemon-owned actor facade for the Preview 1 compatibility backend.
pub struct Preview1CompatActor {
    name: String,
    runtime: Preview1WorkerRuntime,
}

impl Preview1CompatActor {
    /// Load a marker-bearing compatibility actor.
    pub async fn load(
        path: impl AsRef<Path>,
        capabilities: Preview1ActorCapabilities,
        config: Preview1WorkerConfig,
    ) -> Result<Self, Preview1WorkerError> {
        let path = path.as_ref();
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("preview1-compat")
            .to_string();
        let runtime = Preview1WorkerRuntime::load(path, capabilities, config).await?;
        Ok(Self { name, runtime })
    }

    /// Load with the daemon's shared native filesystem watcher service.
    pub async fn load_with_watcher_service(
        path: impl AsRef<Path>,
        capabilities: Preview1ActorCapabilities,
        config: Preview1WorkerConfig,
        watcher_service: NativeFilesystemWatcherService,
    ) -> Result<Self, Preview1WorkerError> {
        let path = path.as_ref();
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("preview1-compat")
            .to_string();
        let runtime = Preview1WorkerRuntime::load_with_watcher_service(
            path,
            capabilities,
            config,
            watcher_service,
        )
        .await?;
        Ok(Self { name, runtime })
    }

    /// Actor name derived from the plugin artifact.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Nonblocking normal-mailbox enqueue.
    pub fn try_send_message(
        &self,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<(), Preview1WorkerError> {
        self.runtime.try_send_message(sender, payload)
    }

    /// Current lifecycle counters, including the reload generation.
    pub fn stats(&self) -> Preview1CompatStats {
        self.runtime.stats()
    }

    pub fn filesystem_watch_stats(&self) -> crate::filesystem_watch::FilesystemWatchStats {
        self.runtime.filesystem_watch_stats()
    }

    pub fn runtime_io_stats(&self) -> crate::runtime_io::RuntimeIoStats {
        self.runtime.runtime_io_stats()
    }

    pub const fn generation(&self) -> u64 {
        self.runtime.generation
    }

    pub async fn lifecycle(
        &self,
        event: PluginLifecycleEvent,
        timeout: Duration,
    ) -> Result<(), Preview1WorkerError> {
        let (reply, response) = oneshot::channel();
        self.runtime
            .commands
            .send(Command::Lifecycle { event, reply })
            .await
            .map_err(|_closed| Preview1WorkerError::RuntimeStopped)?;
        wait_for_lifecycle_reply(response, timeout)
            .await
            .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?
    }

    /// Consume a terminal dispatcher error for daemon status reporting.
    pub fn try_take_failure(&self) -> Option<String> {
        self.runtime.try_take_terminal_error()
    }

    /// Call an actor export while preserving actor-store serialization.
    pub async fn call_actor(
        &self,
        export: impl Into<String>,
        argument: u32,
    ) -> Result<u32, Preview1WorkerError> {
        self.runtime.call_actor(export, argument).await
    }

    /// Cancel workers and await their Stores plus the completion supervisor.
    pub async fn shutdown(&mut self) -> Result<(), Preview1WorkerError> {
        self.runtime.shutdown().await
    }
}

/// Return true when a core module explicitly opts into the compatibility ABI.
pub fn declares_preview1_compat(module: &Module) -> bool {
    module.exports().any(|export| {
        export.name() == COMPAT_MARKER_EXPORT && matches!(export.ty(), ExternType::Func(_))
    })
}

enum Command {
    Mailbox {
        sender: String,
        payload: Vec<u8>,
        permit: OwnedSemaphorePermit,
        byte_permit: GenerationBytePermit,
    },
    ActorCall {
        export: String,
        argument: u32,
        reply: oneshot::Sender<Result<u32, Preview1WorkerError>>,
    },
    SpawnWorker {
        export: String,
        argument: u32,
        stdin: Option<WorkerStdin>,
        reply: oneshot::Sender<Result<Preview1Worker, Preview1WorkerError>>,
    },
    GuestSpawnWorker {
        worker: Preview1WorkerId,
        argument: u32,
        stdin: Option<WorkerStdin>,
        input_pump: Option<(DuplexStream, mpsc::Receiver<Vec<u8>>)>,
    },
    GuestCancelWorker(Preview1WorkerId),
    Lifecycle {
        event: PluginLifecycleEvent,
        reply: oneshot::Sender<Result<(), Preview1WorkerError>>,
    },
    GuestLifecycleComplete(std::result::Result<(), String>),
    CancelWorker {
        worker: Preview1WorkerId,
        reply: oneshot::Sender<Result<(), Preview1WorkerError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

enum ActorOperation {
    Dispatch(CompatEvent),
    Mailbox {
        event: CompatEvent,
        permit: OwnedSemaphorePermit,
        byte_permit: GenerationBytePermit,
    },
    Call {
        export: String,
        argument: u32,
        reply: oneshot::Sender<Result<u32, Preview1WorkerError>>,
    },
    PrepareWorker {
        worker: Preview1WorkerId,
        export: String,
        argument: u32,
        stdin: Option<WorkerStdin>,
        reply: Option<oneshot::Sender<Result<Preview1Worker, Preview1WorkerError>>>,
    },
    ReleaseWorker {
        worker: Preview1WorkerId,
        reservation: WorkerReservation,
    },
}

struct CompletionDispatchLane {
    operations: VecDeque<ActorOperation>,
    capacity: usize,
}

impl CompletionDispatchLane {
    fn new(capacity: usize) -> Self {
        Self {
            operations: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn can_receive(&self) -> bool {
        self.operations.len() < self.capacity
    }

    fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    fn push(&mut self, completion: ActorCompletion) {
        assert!(
            self.can_receive(),
            "completion receiver was polled without reserved lane capacity"
        );
        self.operations
            .push_back(ActorOperation::Dispatch(CompatEvent::Completion(
                completion,
            )));
    }

    fn pop(&mut self) -> Option<ActorOperation> {
        self.operations.pop_front()
    }

    fn clear(&mut self) {
        self.operations.clear();
    }
}

fn pop_preview_actor_operation(
    operations: &mut VecDeque<ActorOperation>,
    prefer_mailbox: &mut bool,
) -> Option<ActorOperation> {
    let selected = if *prefer_mailbox {
        operations
            .iter()
            .position(|operation| matches!(operation, ActorOperation::Mailbox { .. }))
            .and_then(|index| operations.remove(index))
    } else {
        None
    };
    *prefer_mailbox = !*prefer_mailbox;
    selected.or_else(|| operations.pop_front())
}

fn pop_preview_dispatch_operation(
    actor_operations: &mut VecDeque<ActorOperation>,
    completion_operations: &mut CompletionDispatchLane,
    prefer_completion: &mut bool,
    prefer_mailbox: &mut bool,
) -> Option<ActorOperation> {
    let completion_first = *prefer_completion && !completion_operations.is_empty();
    *prefer_completion = !*prefer_completion;
    if completion_first || actor_operations.is_empty() {
        completion_operations.pop()
    } else {
        pop_preview_actor_operation(actor_operations, prefer_mailbox)
    }
}

const fn should_dispatch_notification(
    notification_first: bool,
    lanes_empty: (bool, bool, bool),
) -> bool {
    let (notification_operations_empty, actor_operations_empty, completion_operations_empty) =
        lanes_empty;
    !notification_operations_empty
        && (notification_first || (actor_operations_empty && completion_operations_empty))
}

type GuestCall = Pin<Box<dyn Future<Output = CallDone> + Send>>;
type InputPump = Pin<Box<dyn Future<Output = Preview1WorkerId> + Send>>;

async fn run_worker_input_pump(
    worker: Preview1WorkerId,
    mut input: DuplexStream,
    mut receiver: mpsc::Receiver<Vec<u8>>,
) -> Preview1WorkerId {
    while let Some(bytes) = receiver.recv().await {
        if input.write_all(&bytes).await.is_err() {
            break;
        }
    }
    worker
}

enum CallDone {
    ActorDispatched {
        store: Store<Preview1Store>,
        kind: u32,
        result: Result<(), Preview1WorkerError>,
    },
    ActorCall {
        store: Store<Preview1Store>,
        reply: oneshot::Sender<Result<u32, Preview1WorkerError>>,
        result: Result<u32, Preview1WorkerError>,
    },
    WorkerPrepared {
        actor_store: Store<Preview1Store>,
        worker: Preview1WorkerId,
        reply: Option<oneshot::Sender<Result<Preview1Worker, Preview1WorkerError>>>,
        result: Result<PreparedWorker, Preview1WorkerError>,
    },
    WorkerFinished {
        worker: Preview1WorkerId,
        reservation: WorkerReservation,
        result: Result<u32, Preview1WorkerError>,
    },
    WorkerReleased {
        actor_store: Store<Preview1Store>,
        worker: Preview1WorkerId,
        result: Result<(), Preview1WorkerError>,
    },
}

struct PreparedWorker {
    store: Store<Preview1Store>,
    instance: Instance,
    export: String,
    argument: u32,
    reservation: WorkerReservation,
}

struct ActiveWorker {
    abort: AbortHandle,
    completion: Option<oneshot::Sender<Result<u32, Preview1WorkerError>>>,
    guest_owned: bool,
}

#[derive(Clone, Copy, Debug)]
struct WorkerReservation {
    encoded: i64,
    stack_bytes: u32,
    tls_bytes: u32,
    tls_align: u32,
    stack_bottom: u32,
    stack_top: u32,
    tls_base: u32,
    end: u32,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the parent task explicitly owns every actor-scoped runtime resource"
)]
async fn parent_loop(
    engine: Engine,
    module: Module,
    memory: SharedMemory,
    actor_store: Store<Preview1Store>,
    actor_instance: Instance,
    capabilities: Preview1ActorCapabilities,
    mut completion_rx: mpsc::Receiver<ActorCompletion>,
    mut notification_rx: mpsc::Receiver<NotificationId>,
    coordination: CoordinationHub,
    runtime_io: RuntimeIoHub,
    mut completion_supervisor: Preview1CompletionSupervisor,
    next_worker: Arc<AtomicU64>,
    worker_inputs: WorkerInputRegistry,
    stats: Arc<Preview1CompatStatsInner>,
    config: Preview1WorkerConfig,
    mut commands: mpsc::Receiver<Command>,
    _epoch_ticker: EpochTicker,
) -> Result<(), Preview1WorkerError> {
    let completion_io = completion_supervisor.io().clone();
    let mut actor_store = Some(actor_store);
    let mut actor_operations = VecDeque::new();
    let mut completion_operations = CompletionDispatchLane::new(config.max_pending_completions);
    let mut notification_operations = VecDeque::new();
    let mut prefer_notification = true;
    let mut prefer_completion = true;
    let mut prefer_mailbox = true;
    let mut calls = FuturesUnordered::<GuestCall>::new();
    let mut input_pumps = FuturesUnordered::<InputPump>::new();
    let mut active_workers = HashMap::<Preview1WorkerId, ActiveWorker>::new();
    let mut preparing_workers = HashSet::<Preview1WorkerId>::new();
    let mut pending_cancellations = HashSet::<Preview1WorkerId>::new();
    let mut reservations = HashMap::<Preview1WorkerId, WorkerReservation>::new();
    let mut worker_slots = 0_usize;
    let mut completion_open = true;
    let mut shutdown_reply: Option<oneshot::Sender<()>> = None;
    let mut pending_lifecycle: Option<oneshot::Sender<Result<(), Preview1WorkerError>>> = None;

    let loop_result = loop {
        if shutdown_reply.is_some()
            && active_workers.is_empty()
            && preparing_workers.is_empty()
            && reservations.is_empty()
        {
            break Ok(());
        }

        let stopping = shutdown_reply.is_some();
        let release_pending = actor_operations
            .iter()
            .any(|operation| matches!(operation, ActorOperation::ReleaseWorker { .. }));
        let actor_dispatch_available = actor_store.is_some() && (!stopping || release_pending);
        let actor_work_pending = !actor_operations.is_empty()
            || !completion_operations.is_empty()
            || !notification_operations.is_empty();
        if actor_dispatch_available && actor_work_pending {
            let operation = if stopping {
                let Some(index) = actor_operations.iter().position(|operation| {
                    matches!(operation, ActorOperation::ReleaseWorker { .. })
                }) else {
                    continue;
                };
                let Some(operation) = actor_operations.remove(index) else {
                    continue;
                };
                operation
            } else {
                let notification_first = prefer_notification && !notification_operations.is_empty();
                prefer_notification = !prefer_notification;
                if should_dispatch_notification(
                    notification_first,
                    (
                        notification_operations.is_empty(),
                        actor_operations.is_empty(),
                        completion_operations.is_empty(),
                    ),
                ) {
                    let Some(notification) = notification_operations.pop_front() else {
                        continue;
                    };
                    let Some(event) = coordination
                        .take_actor_event(notification)
                        .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?
                    else {
                        continue;
                    };
                    ActorOperation::Dispatch(CompatEvent::Notification(event))
                } else {
                    let Some(operation) = pop_preview_dispatch_operation(
                        &mut actor_operations,
                        &mut completion_operations,
                        &mut prefer_completion,
                        &mut prefer_mailbox,
                    ) else {
                        continue;
                    };
                    operation
                }
            };
            let Some(store) = actor_store.take() else {
                continue;
            };
            calls.push(start_actor_operation(
                operation,
                store,
                actor_instance,
                engine.clone(),
                module.clone(),
                memory.clone(),
                capabilities.clone(),
                config.worker_stack_bytes,
                config.actor_yield_ticks,
                config.worker_yield_ticks,
                reservations.values().copied().collect(),
                config.limits,
            ));
        }

        tokio::select! {
            completion = completion_rx.recv(), if completion_open
                && completion_operations.can_receive() => {
                match completion {
                    Some(completion) => completion_operations.push(completion),
                    None => completion_open = false,
                }
            }
            notification = notification_rx.recv() => {
                if let Some(notification) = notification {
                    notification_operations.push_back(notification);
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    cancel_all_workers(&mut active_workers);
                    coordination.stop();
                    completion_io.stop();
                    prepare_actor_operations_for_shutdown(
                        &mut actor_operations,
                        &mut preparing_workers,
                        &worker_inputs,
                        &mut worker_slots,
                    );
                    shutdown_reply = Some(oneshot::channel().0);
                    continue;
                };
                if shutdown_reply.is_some() {
                    if let Command::Shutdown { reply } = command {
                        let _result = reply.send(());
                    }
                    continue;
                }
                match command {
                    Command::Lifecycle { event, reply } => {
                        if pending_lifecycle.is_some() {
                            let _result = reply.send(Err(Preview1WorkerError::Guest(
                                "lifecycle hook already pending".to_string(),
                            )));
                        } else {
                            pending_lifecycle = Some(reply);
                            actor_operations.push_front(ActorOperation::Dispatch(
                                CompatEvent::Lifecycle(event),
                            ));
                        }
                    }
                    Command::GuestLifecycleComplete(result) => {
                        let Some(reply) = pending_lifecycle.take() else {
                            break Err(Preview1WorkerError::Guest(
                                "guest completed lifecycle with no pending hook".to_string(),
                            ));
                        };
                        let _result = reply.send(result.map_err(Preview1WorkerError::Guest));
                    }
                    Command::Mailbox { sender, payload, permit, byte_permit } => {
                        // Admission was reserved before the public enqueue
                        // returned success. Retain it through dispatch so an
                        // accepted mailbox message cannot be silently dropped.
                        actor_operations.push_back(ActorOperation::Mailbox {
                            event: CompatEvent::Mailbox { sender, payload },
                            permit,
                            byte_permit,
                        });
                    }
                    Command::ActorCall { export, argument, reply } => {
                        if actor_operations.len() >= config.max_pending_actor_operations {
                            let _result = reply.send(Err(Preview1WorkerError::AtCapacity(
                                "pending actor operations",
                            )));
                        } else {
                            actor_operations.push_back(ActorOperation::Call {
                                export,
                                argument,
                                reply,
                            });
                        }
                    }
                    Command::SpawnWorker { export, argument, stdin, reply } => {
                        if worker_slots >= config.max_workers {
                            let _result = reply.send(Err(Preview1WorkerError::AtCapacity("workers")));
                        } else if actor_operations.len() >= config.max_pending_actor_operations {
                            let _result = reply.send(Err(Preview1WorkerError::AtCapacity(
                                "pending actor operations",
                            )));
                        } else {
                            let Ok(worker) = allocate_worker_id(&next_worker) else {
                                let _result = reply.send(Err(Preview1WorkerError::AtCapacity(
                                    "worker identifiers",
                                )));
                                continue;
                            };
                            worker_slots += 1;
                            preparing_workers.insert(worker);
                            actor_operations.push_back(ActorOperation::PrepareWorker {
                                worker,
                                export,
                                argument,
                                stdin,
                                reply: Some(reply),
                            });
                        }
                    }
                    Command::GuestSpawnWorker { worker, argument, stdin, input_pump } => {
                        if worker_slots < config.max_workers
                            && actor_operations.len() < config.max_pending_actor_operations
                        {
                            worker_slots += 1;
                            preparing_workers.insert(worker);
                            actor_operations.push_back(ActorOperation::PrepareWorker {
                                worker,
                                export: COMPAT_WORKER_EXPORT.to_string(),
                                argument,
                                stdin,
                                reply: None,
                            });
                            if let Some((input, input_receiver)) = input_pump {
                                input_pumps.push(Box::pin(run_worker_input_pump(
                                    worker,
                                    input,
                                    input_receiver,
                                )));
                            }
                        } else {
                            remove_worker_input(&worker_inputs, worker);
                            actor_operations.push_back(ActorOperation::Dispatch(
                                CompatEvent::WorkerFailed {
                                    worker,
                                    message: "worker capacity reached".to_string(),
                                },
                            ));
                        }
                    }
                    Command::GuestCancelWorker(worker) => {
                        if let Some(active) = active_workers.get_mut(&worker) {
                            active.abort.abort();
                        } else if preparing_workers.contains(&worker) {
                            // Cancellation remains sticky until preparation
                            // yields ownership of the worker Store back here.
                            pending_cancellations.insert(worker);
                        } else if remove_queued_worker_failure(&mut actor_operations, worker) {
                            // Cancellation was accepted before an admission
                            // failure was dispatched, so cancellation wins.
                            actor_operations.push_back(ActorOperation::Dispatch(
                                CompatEvent::WorkerCancelled(worker),
                            ));
                        }
                    }
                    Command::CancelWorker { worker, reply } => {
                        let result = if let Some(active) = active_workers.get_mut(&worker) {
                            active.abort.abort();
                            Ok(())
                        } else {
                            Err(Preview1WorkerError::Guest(format!(
                                "worker {} is not active",
                                worker.raw(),
                            )))
                        };
                        let _result = reply.send(result);
                    }
                    Command::Shutdown { reply } => {
                        cancel_all_workers(&mut active_workers);
                        coordination.stop();
                        prepare_actor_operations_for_shutdown(
                            &mut actor_operations,
                            &mut preparing_workers,
                            &worker_inputs,
                            &mut worker_slots,
                        );
                        if let Ok(mut inputs) = worker_inputs.lock() {
                            inputs.clear();
                        }
                        completion_operations.clear();
                        notification_operations.clear();
                        completion_io.stop();
                        completion_open = false;
                        shutdown_reply = Some(reply);
                    }
                }
            }
            done = calls.next(), if !calls.is_empty() => {
                let Some(done) = done else {
                    continue;
                };
                match done {
                    CallDone::ActorDispatched { store, kind, result } => {
                        actor_store = Some(store);
                        if let Err(error) = result {
                            break Err(error);
                        }
                        match kind {
                            EVENT_MAILBOX => {
                                stats.mailbox_dispatched.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_COMPLETION_READ
                            | EVENT_COMPLETION_WRITE
                            | EVENT_COMPLETION_CANCELLED
                            | EVENT_COMPLETION_FAILED => {
                                stats.completion_delivered.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_WORKER_STARTED => {
                                stats.workers_started.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_WORKER_FINISHED => {
                                stats.workers_finished.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_WORKER_FAILED => {
                                stats.workers_failed.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_WORKER_CANCELLED => {
                                stats.workers_cancelled.fetch_add(1, Ordering::Relaxed);
                            }
                            EVENT_NOTIFICATION => {
                                stats
                                    .notifications_delivered
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                    CallDone::ActorCall { store, reply, result } => {
                        actor_store = Some(store);
                        let _result = reply.send(result);
                    }
                    CallDone::WorkerPrepared { actor_store: store, worker, reply, result } => {
                        actor_store = Some(store);
                        preparing_workers.remove(&worker);
                        let guest_owned = reply.is_none();
                        let cancelled = shutdown_reply.is_some()
                            || pending_cancellations.remove(&worker);
                        match result {
                            Ok(prepared) => {
                                if cancelled {
                                    worker_slots = worker_slots.saturating_sub(1);
                                    let reservation = prepared.reservation;
                                    // A cancellation completion is not visible
                                    // until the prepared worker Store is gone.
                                    drop(prepared);
                                    remove_worker_input(&worker_inputs, worker);
                                    reservations.insert(worker, reservation);
                                    actor_operations.push_front(ActorOperation::ReleaseWorker {
                                        worker,
                                        reservation,
                                    });
                                    if let Some(reply) = reply {
                                        let _result = reply.send(Err(Preview1WorkerError::Cancelled));
                                    }
                                    if shutdown_reply.is_none() {
                                        actor_operations.push_front(ActorOperation::Dispatch(
                                            CompatEvent::WorkerCancelled(worker),
                                        ));
                                    } else {
                                        stats.workers_cancelled.fetch_add(1, Ordering::Relaxed);
                                    }
                                } else if overlaps_existing(prepared.reservation, reservations.values().copied()) {
                                    worker_slots = worker_slots.saturating_sub(1);
                                    actor_operations.push_front(ActorOperation::ReleaseWorker {
                                        worker,
                                        reservation: prepared.reservation,
                                    });
                                    if let Some(reply) = reply {
                                        let _result = reply.send(Err(Preview1WorkerError::Guest(
                                            "guest allocator returned an overlapping worker reservation".to_string(),
                                        )));
                                    }
                                    if guest_owned {
                                        remove_worker_input(&worker_inputs, worker);
                                        actor_operations.push_front(ActorOperation::Dispatch(
                                            CompatEvent::WorkerFailed {
                                                worker,
                                                message: "worker reservation overlapped".to_string(),
                                            },
                                        ));
                                    }
                                } else {
                                    reservations.insert(worker, prepared.reservation);
                                    let started = start_prepared_worker(
                                        prepared,
                                        worker,
                                        reply,
                                        &mut active_workers,
                                        &calls,
                                        &mut actor_operations,
                                    );
                                    if started {
                                        stats.workers_active.fetch_add(1, Ordering::AcqRel);
                                    } else {
                                        worker_slots = worker_slots.saturating_sub(1);
                                        if guest_owned {
                                            remove_worker_input(&worker_inputs, worker);
                                            actor_operations.push_front(ActorOperation::Dispatch(
                                                CompatEvent::WorkerFailed {
                                                    worker,
                                                    message: "worker export could not start".to_string(),
                                                },
                                            ));
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                worker_slots = worker_slots.saturating_sub(1);
                                let message = error.to_string();
                                if let Some(reply) = reply {
                                    let result = if cancelled {
                                        Err(Preview1WorkerError::Cancelled)
                                    } else {
                                        Err(error)
                                    };
                                    let _result = reply.send(result);
                                }
                                if guest_owned {
                                    remove_worker_input(&worker_inputs, worker);
                                    if shutdown_reply.is_none() {
                                        actor_operations.push_front(ActorOperation::Dispatch(
                                            if cancelled {
                                                CompatEvent::WorkerCancelled(worker)
                                            } else {
                                                CompatEvent::WorkerFailed { worker, message }
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    CallDone::WorkerFinished { worker, reservation, result } => {
                        worker_slots = worker_slots.saturating_sub(1);
                        remove_worker_input(&worker_inputs, worker);
                        if let Some(mut active) = active_workers.remove(&worker) {
                            stats.workers_active.fetch_sub(1, Ordering::AcqRel);
                            if active.guest_owned
                                && let Ok(value) = &result
                            {
                                stats
                                    .last_worker_value
                                    .store(u64::from(*value), Ordering::Relaxed);
                            }
                            let event = match &result {
                                Ok(value) => CompatEvent::WorkerFinished {
                                    worker,
                                    value: *value,
                                },
                                Err(Preview1WorkerError::Cancelled) => {
                                    CompatEvent::WorkerCancelled(worker)
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        worker = worker.raw(),
                                        error = %error,
                                        "Preview 1 compatibility worker failed"
                                    );
                                    CompatEvent::WorkerFailed {
                                        worker,
                                        message: error.to_string(),
                                    }
                                }
                            };
                            if let Some(completion) = active.completion.take() {
                                let _result = completion.send(result);
                            }
                            if active.guest_owned {
                                actor_operations.push_front(ActorOperation::Dispatch(event));
                            }
                        }
                        actor_operations.push_front(ActorOperation::ReleaseWorker {
                            worker,
                            reservation,
                        });
                    }
                    CallDone::WorkerReleased { actor_store: store, worker, result } => {
                        actor_store = Some(store);
                        if let Err(error) = result {
                            break Err(error);
                        }
                        reservations.remove(&worker);
                    }
                }
            }
            _worker = input_pumps.next(), if !input_pumps.is_empty() => {}
        }
    };

    // Dropping every guest call and the completion receiver first ensures no
    // Store retains an I/O handle and no supervisor send can block teardown.
    drop(calls);
    drop(actor_store);
    drop(actor_operations);
    drop(completion_operations);
    drop(notification_operations);
    drop(completion_rx);
    drop(notification_rx);
    runtime_io.teardown().await;
    coordination.stop();
    stats.workers_active.store(0, Ordering::Release);
    completion_supervisor.shutdown().await;
    if let Some(reply) = shutdown_reply.take() {
        let _result = reply.send(());
    }
    loop_result
}

fn start_dispatch_operation(
    mut actor_store: Store<Preview1Store>,
    actor_instance: Instance,
    event: CompatEvent,
    mailbox_permit: Option<(OwnedSemaphorePermit, GenerationBytePermit)>,
    actor_yield_ticks: u64,
) -> GuestCall {
    Box::pin(async move {
        // Keep mailbox admission until the accepted event has a terminal guest
        // dispatch result. Non-mailbox events carry no permit.
        let _mailbox_permit = mailbox_permit;
        if let CompatEvent::WorkerFailed { worker, message } = &event {
            tracing::warn!(
                worker = worker.raw(),
                error = %message,
                "delivering Preview 1 worker failure to its actor"
            );
        }
        configure_actor_yield(&mut actor_store, actor_yield_ticks);
        let kind = event.kind();
        actor_store.data_mut().host.event = Some(event);
        let result = async {
            let function = actor_instance
                .get_typed_func::<u32, u32>(&mut actor_store, COMPAT_DISPATCH_EXPORT)
                .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
            let status = function
                .call_async(&mut actor_store, kind)
                .await
                .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
            if status != 0 {
                return Err(Preview1WorkerError::Guest(format!(
                    "{COMPAT_DISPATCH_EXPORT} returned status {status}"
                )));
            }
            Ok(())
        }
        .await;
        actor_store.data_mut().host.event = None;
        CallDone::ActorDispatched {
            store: actor_store,
            kind,
            result,
        }
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "building an owned call future requires the actor resources and bounded policy"
)]
fn start_actor_operation(
    operation: ActorOperation,
    mut actor_store: Store<Preview1Store>,
    actor_instance: Instance,
    engine: Engine,
    module: Module,
    memory: SharedMemory,
    capabilities: Preview1ActorCapabilities,
    stack_bytes: u32,
    actor_yield_ticks: u64,
    worker_yield_ticks: u64,
    existing_reservations: Vec<WorkerReservation>,
    limits: WasmResourceLimits,
) -> GuestCall {
    match operation {
        ActorOperation::Dispatch(event) => {
            start_dispatch_operation(actor_store, actor_instance, event, None, actor_yield_ticks)
        }
        ActorOperation::Mailbox {
            event,
            permit,
            byte_permit,
        } => start_dispatch_operation(
            actor_store,
            actor_instance,
            event,
            Some((permit, byte_permit)),
            actor_yield_ticks,
        ),
        ActorOperation::Call {
            export,
            argument,
            reply,
        } => Box::pin(async move {
            configure_actor_yield(&mut actor_store, actor_yield_ticks);
            let result = async {
                let function = actor_instance
                    .get_typed_func::<u32, u32>(&mut actor_store, &export)
                    .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
                function
                    .call_async(&mut actor_store, argument)
                    .await
                    .map_err(|error| Preview1WorkerError::Guest(error.to_string()))
            }
            .await;
            CallDone::ActorCall {
                store: actor_store,
                reply,
                result,
            }
        }),
        ActorOperation::PrepareWorker {
            worker,
            export,
            argument,
            stdin,
            reply,
        } => Box::pin(async move {
            configure_actor_yield(&mut actor_store, actor_yield_ticks);
            let result = prepare_worker(
                &engine,
                &module,
                &memory,
                &mut actor_store,
                actor_instance,
                &capabilities,
                stdin,
                export,
                argument,
                stack_bytes,
                worker_yield_ticks,
                &existing_reservations,
                limits,
            )
            .await;
            CallDone::WorkerPrepared {
                actor_store,
                worker,
                reply,
                result,
            }
        }),
        ActorOperation::ReleaseWorker {
            worker,
            reservation,
        } => Box::pin(async move {
            configure_actor_yield(&mut actor_store, actor_yield_ticks);
            let result = release_worker(&mut actor_store, actor_instance, reservation).await;
            CallDone::WorkerReleased {
                actor_store,
                worker,
                result,
            }
        }),
    }
}

fn start_prepared_worker(
    prepared: PreparedWorker,
    worker: Preview1WorkerId,
    reply: Option<oneshot::Sender<Result<Preview1Worker, Preview1WorkerError>>>,
    active_workers: &mut HashMap<Preview1WorkerId, ActiveWorker>,
    calls: &FuturesUnordered<GuestCall>,
    actor_operations: &mut VecDeque<ActorOperation>,
) -> bool {
    let guest_owned = reply.is_none();
    let PreparedWorker {
        mut store,
        instance,
        export,
        argument,
        reservation,
    } = prepared;
    let function = match instance.get_typed_func::<u32, u32>(&mut store, &export) {
        Ok(function) => function,
        Err(error) => {
            actor_operations.push_front(ActorOperation::ReleaseWorker {
                worker,
                reservation,
            });
            if let Some(reply) = reply {
                let _result = reply.send(Err(Preview1WorkerError::Guest(error.to_string())));
            }
            return false;
        }
    };
    let completion = if let Some(reply) = reply {
        let (completion, receiver) = oneshot::channel();
        let handle = Preview1Worker {
            id: worker,
            completion: receiver,
        };
        if reply.send(Ok(handle)).is_err() {
            actor_operations.push_front(ActorOperation::ReleaseWorker {
                worker,
                reservation,
            });
            return false;
        }
        Some(completion)
    } else {
        None
    };

    let (abort, registration) = AbortHandle::new_pair();
    active_workers.insert(
        worker,
        ActiveWorker {
            abort,
            completion,
            guest_owned,
        },
    );
    calls.push(Box::pin(async move {
        let call = async {
            function
                .call_async(&mut store, argument)
                .await
                .map_err(|error| Preview1WorkerError::Guest(error.to_string()))
        };
        let result = match Abortable::new(call, registration).await {
            Ok(result) => result,
            Err(_aborted) => Err(Preview1WorkerError::Cancelled),
        };
        drop(store);
        CallDone::WorkerFinished {
            worker,
            reservation,
            result,
        }
    }));
    if guest_owned {
        // `started` means admission and Store/export preparation succeeded and
        // the worker call is now owned by the parent scheduler. Failures before
        // this point produce only a typed `worker-failed` event.
        actor_operations.push_front(ActorOperation::Dispatch(CompatEvent::WorkerStarted(worker)));
    }
    true
}

#[allow(
    clippy::too_many_arguments,
    reason = "worker preparation validates actor, module, WASI, and reservation state together"
)]
async fn prepare_worker(
    engine: &Engine,
    module: &Module,
    memory: &SharedMemory,
    actor_store: &mut Store<Preview1Store>,
    actor_instance: Instance,
    capabilities: &Preview1ActorCapabilities,
    stdin: Option<WorkerStdin>,
    export: String,
    argument: u32,
    stack_bytes: u32,
    worker_yield_ticks: u64,
    existing_reservations: &[WorkerReservation],
    limits: WasmResourceLimits,
) -> Result<PreparedWorker, Preview1WorkerError> {
    // Read the immutable TLS metadata from the actor instance so the actor can
    // reserve memory before the secondary instance's start function runs.
    let tls_size = global_u32_optional(actor_store, actor_instance, "__tls_size")?;
    let tls_alignment = global_u32_optional(actor_store, actor_instance, "__tls_align")?;
    let (tls_bytes, tls_align) = match (tls_size, tls_alignment) {
        (None, None) => (0, 1),
        (Some(bytes), Some(align)) => (bytes, align),
        _ => {
            return Err(Preview1WorkerError::InvalidModule(
                "__tls_size and __tls_align must either both be exported or both be absent"
                    .to_string(),
            ));
        }
    };
    if tls_align == 0 || !tls_align.is_power_of_two() {
        return Err(Preview1WorkerError::InvalidModule(
            "__tls_align must be a non-zero power of two".to_string(),
        ));
    }

    let reserve = actor_instance
        .get_typed_func::<(u32, u32, u32), i64>(&mut *actor_store, WORKER_RESERVE_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    let encoded = reserve
        .call_async(&mut *actor_store, (stack_bytes, tls_bytes, tls_align))
        .await
        .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    let reservation = decode_reservation(
        encoded,
        stack_bytes,
        tls_bytes,
        tls_align,
        memory.data_size(),
    )?;
    if overlaps_existing(reservation, existing_reservations.iter().copied()) {
        release_worker(actor_store, actor_instance, reservation).await?;
        return Err(Preview1WorkerError::Guest(
            "guest allocator returned an overlapping worker reservation".to_string(),
        ));
    }

    let (
        worker_completion_io,
        worker_coordination,
        worker_filesystem_watches,
        worker_runtime_io,
        worker_runtime_children,
        worker_commands,
        worker_next_id,
        worker_input_capacity,
        worker_input_backlog,
        worker_inputs,
        worker_stats,
    ) = {
        let actor_host = &actor_store.data().host;
        (
            actor_host.completion_io.clone(),
            actor_host.coordination.clone(),
            actor_host.filesystem_watches.clone(),
            actor_host.runtime_io.clone(),
            Arc::clone(&actor_host.runtime_children),
            actor_host.commands.clone(),
            Arc::clone(&actor_host.next_worker),
            actor_host.worker_input_capacity,
            actor_host.worker_input_backlog,
            Arc::clone(&actor_host.worker_inputs),
            Arc::clone(&actor_host.stats),
        )
    };
    let prepared = async move {
        let wasi = build_wasi_context(capabilities, stdin)?;
        let host = Preview1Host {
            actor: false,
            event: None,
            completion_io: worker_completion_io,
            coordination: worker_coordination,
            filesystem_watches: worker_filesystem_watches,
            runtime_io: worker_runtime_io,
            runtime_children: worker_runtime_children,
            commands: worker_commands,
            next_worker: worker_next_id,
            worker_input_capacity,
            worker_input_backlog,
            worker_inputs,
            stats: worker_stats,
        };
        let mut store = Store::new(
            engine,
            Preview1Store {
                wasi,
                host,
                limits: limits.store_limits(),
            },
        );
        store.limiter(|data| &mut data.limits);
        store.set_epoch_deadline(worker_yield_ticks);
        store.epoch_deadline_async_yield_and_update(worker_yield_ticks);
        let instance = instantiate(engine, module, memory, &mut store).await?;
        configure_worker_context(&mut store, instance, reservation).await?;
        Ok(PreparedWorker {
            store,
            instance,
            export,
            argument,
            reservation,
        })
    }
    .await;
    if prepared.is_err() {
        release_worker(actor_store, actor_instance, reservation).await?;
    }
    prepared
}

async fn configure_worker_context(
    store: &mut Store<Preview1Store>,
    instance: Instance,
    reservation: WorkerReservation,
) -> Result<(), Preview1WorkerError> {
    instance
        .get_global(&mut *store, "__stack_pointer")
        .ok_or_else(|| {
            Preview1WorkerError::InvalidModule("missing __stack_pointer export".to_string())
        })?
        .set(
            &mut *store,
            Val::I32(i32::from_ne_bytes(reservation.stack_top.to_ne_bytes())),
        )
        .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    if reservation.tls_bytes != 0 {
        let init_tls = instance
            .get_typed_func::<u32, ()>(&mut *store, "__wasm_init_tls")
            .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
        init_tls
            .call_async(&mut *store, reservation.tls_base)
            .await
            .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    }

    if global_u32(store, instance, "__stack_pointer")? != reservation.stack_top
        || (reservation.tls_bytes != 0
            && global_u32(store, instance, "__tls_base")? != reservation.tls_base)
    {
        return Err(Preview1WorkerError::Guest(
            "worker stack/TLS globals did not retain their reservation".to_string(),
        ));
    }
    Ok(())
}

async fn release_worker(
    actor_store: &mut Store<Preview1Store>,
    actor_instance: Instance,
    reservation: WorkerReservation,
) -> Result<(), Preview1WorkerError> {
    let release = actor_instance
        .get_typed_func::<(i64, u32, u32, u32), u32>(&mut *actor_store, WORKER_RELEASE_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    let status = release
        .call_async(
            &mut *actor_store,
            (
                reservation.encoded,
                reservation.stack_bytes,
                reservation.tls_bytes,
                reservation.tls_align,
            ),
        )
        .await
        .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    if status != 0 {
        return Err(Preview1WorkerError::Guest(format!(
            "{WORKER_RELEASE_EXPORT} returned status {status}"
        )));
    }
    Ok(())
}

fn build_wasi_context(
    capabilities: &Preview1ActorCapabilities,
    stdin: Option<WorkerStdin>,
) -> Result<WasiP1Ctx, Preview1WorkerError> {
    let mut builder = WasiCtxBuilder::new();
    if let Some(stdin) = stdin {
        builder.stdin(stdin);
    }
    if let Some(filesystem) = &capabilities.filesystem {
        let (directory_permissions, file_permissions) = if filesystem.writable {
            (DirPerms::all(), FilePerms::all())
        } else {
            (DirPerms::READ, FilePerms::READ)
        };
        builder
            .preopened_dir(
                &filesystem.host_path,
                &filesystem.guest_path,
                directory_permissions,
                file_permissions,
            )
            .map_err(|error| Preview1WorkerError::InvalidCapability(error.to_string()))?;
    }
    Ok(builder.build_p1())
}

async fn instantiate(
    engine: &Engine,
    module: &Module,
    memory: &SharedMemory,
    store: &mut Store<Preview1Store>,
) -> Result<Instance, Preview1WorkerError> {
    let mut linker = Linker::new(engine);
    p1::add_to_linker_async(&mut linker, |context: &mut Preview1Store| &mut context.wasi)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    host_abi::brood_preview1::add_to_linker(&mut linker, |context: &mut Preview1Store| {
        &mut context.host
    })
    .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    linker
        .define(
            &mut *store,
            SHARED_MEMORY_IMPORT_MODULE,
            SHARED_MEMORY_IMPORT_NAME,
            memory.clone(),
        )
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    linker
        .instantiate_async(store, module)
        .await
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))
}

const fn validate_config(config: Preview1WorkerConfig) -> Result<(), Preview1WorkerError> {
    if config.limits.mailbox_sender_bytes == 0
        || config.limits.mailbox_body_bytes == 0
        || config.limits.worker_payload_bytes == 0
        || config.limits.worker_result_bytes == 0
        || config.limits.generation_host_bytes == 0
        || config.limits.generation_memory_bytes == 0
        || config.limits.store_table_elements == 0
    {
        return Err(Preview1WorkerError::InvalidConfig(
            "resource byte and table limits must be non-zero",
        ));
    }
    if config.max_workers == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_workers must be non-zero",
        ));
    }
    if config.max_pending_actor_operations == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_pending_actor_operations must be non-zero",
        ));
    }
    if config.max_pending_completions == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_pending_completions must be non-zero",
        ));
    }
    if config.command_capacity == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "command_capacity must be non-zero",
        ));
    }
    if config.mailbox_backlog == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "mailbox_backlog must be non-zero",
        ));
    }
    if config.max_outstanding_io == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_outstanding_io must be non-zero",
        ));
    }
    if config.max_completion_bytes == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_completion_bytes must be non-zero",
        ));
    }
    if config.max_filesystem_watches == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_filesystem_watches must be non-zero",
        ));
    }
    if config.max_filesystem_watch_events == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_filesystem_watch_events must be non-zero",
        ));
    }
    if config.max_notifications == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_notifications must be non-zero",
        ));
    }
    if config.max_pollables == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_pollables must be non-zero",
        ));
    }
    if config.max_wait_set == 0 || config.max_wait_set > config.max_pollables {
        return Err(Preview1WorkerError::InvalidConfig(
            "max_wait_set must be non-zero and no greater than max_pollables",
        ));
    }
    if config.worker_input_capacity == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "worker_input_capacity must be non-zero",
        ));
    }
    if config.worker_input_backlog == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "worker_input_backlog must be non-zero",
        ));
    }
    if config.worker_stack_bytes < 64 * 1024
        || !config.worker_stack_bytes.is_multiple_of(STACK_ALIGNMENT)
    {
        return Err(Preview1WorkerError::InvalidConfig(
            "worker_stack_bytes must be at least 64 KiB and 16-byte aligned",
        ));
    }
    if config.epoch_tick_interval.is_zero() {
        return Err(Preview1WorkerError::InvalidConfig(
            "epoch_tick_interval must be non-zero",
        ));
    }
    if config.actor_yield_ticks == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "actor_yield_ticks must be non-zero",
        ));
    }
    if config.worker_yield_ticks == 0 {
        return Err(Preview1WorkerError::InvalidConfig(
            "worker_yield_ticks must be non-zero",
        ));
    }
    Ok(())
}

fn configure_actor_yield(store: &mut Store<Preview1Store>, ticks: u64) {
    store.set_epoch_deadline(ticks);
    store.epoch_deadline_async_yield_and_update(ticks);
}

async fn validate_actor_exports(
    store: &mut Store<Preview1Store>,
    instance: Instance,
) -> Result<(), Preview1WorkerError> {
    instance
        .get_typed_func::<(u32, u32, u32), i64>(&mut *store, WORKER_RESERVE_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    instance
        .get_typed_func::<(i64, u32, u32, u32), u32>(&mut *store, WORKER_RELEASE_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    instance
        .get_typed_func::<u32, u32>(&mut *store, COMPAT_DISPATCH_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    instance
        .get_typed_func::<u32, u32>(&mut *store, COMPAT_WORKER_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    let marker = instance
        .get_typed_func::<(), u32>(&mut *store, COMPAT_MARKER_EXPORT)
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    let marker = marker
        .call_async(&mut *store, ())
        .await
        .map_err(|error| Preview1WorkerError::InvalidModule(error.to_string()))?;
    if marker != COMPAT_MARKER_MAGIC {
        return Err(Preview1WorkerError::InvalidModule(format!(
            "{COMPAT_MARKER_EXPORT} returned {marker:#x}, expected {COMPAT_MARKER_MAGIC:#x}"
        )));
    }
    Ok(())
}

fn global_u32(
    store: &mut Store<Preview1Store>,
    instance: Instance,
    name: &str,
) -> Result<u32, Preview1WorkerError> {
    let value = instance
        .get_global(&mut *store, name)
        .ok_or_else(|| Preview1WorkerError::InvalidModule(format!("missing {name} export")))?
        .get(&mut *store)
        .i32()
        .ok_or_else(|| Preview1WorkerError::InvalidModule(format!("{name} must be i32")))?;
    Ok(u32::from_ne_bytes(value.to_ne_bytes()))
}

fn global_u32_optional(
    store: &mut Store<Preview1Store>,
    instance: Instance,
    name: &str,
) -> Result<Option<u32>, Preview1WorkerError> {
    let Some(global) = instance.get_global(&mut *store, name) else {
        return Ok(None);
    };
    let value = global
        .get(&mut *store)
        .i32()
        .ok_or_else(|| Preview1WorkerError::InvalidModule(format!("{name} must be i32")))?;
    Ok(Some(u32::from_ne_bytes(value.to_ne_bytes())))
}

fn decode_reservation(
    encoded: i64,
    stack_bytes: u32,
    tls_bytes: u32,
    tls_align: u32,
    memory_bytes: usize,
) -> Result<WorkerReservation, Preview1WorkerError> {
    if encoded == 0 {
        return Err(Preview1WorkerError::Guest(
            "guest allocator could not reserve worker stack/TLS memory".to_string(),
        ));
    }
    let bits = u64::from_ne_bytes(encoded.to_ne_bytes());
    let stack_top = u32::try_from(bits & u64::from(u32::MAX))
        .map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    let tls_base =
        u32::try_from(bits >> 32).map_err(|error| Preview1WorkerError::Guest(error.to_string()))?;
    let stack_bottom = stack_top.checked_sub(stack_bytes).ok_or_else(|| {
        Preview1WorkerError::Guest("worker stack reservation underflowed".to_string())
    })?;
    let end = tls_base.checked_add(tls_bytes).ok_or_else(|| {
        Preview1WorkerError::Guest("worker TLS reservation overflowed".to_string())
    })?;
    if stack_bottom == 0
        || stack_top > tls_base
        || tls_base % tls_align != 0
        || usize::try_from(end).map_or(true, |end| end > memory_bytes)
    {
        return Err(Preview1WorkerError::Guest(
            "guest returned an invalid worker stack/TLS reservation".to_string(),
        ));
    }
    Ok(WorkerReservation {
        encoded,
        stack_bytes,
        tls_bytes,
        tls_align,
        stack_bottom,
        stack_top,
        tls_base,
        end,
    })
}

fn overlaps_existing(
    candidate: WorkerReservation,
    existing: impl IntoIterator<Item = WorkerReservation>,
) -> bool {
    existing.into_iter().any(|reservation| {
        candidate.stack_bottom < reservation.end && reservation.stack_bottom < candidate.end
    })
}

fn cancel_all_workers(active_workers: &mut HashMap<Preview1WorkerId, ActiveWorker>) {
    for active in active_workers.values_mut() {
        active.abort.abort();
    }
}

fn prepare_actor_operations_for_shutdown(
    actor_operations: &mut VecDeque<ActorOperation>,
    preparing_workers: &mut HashSet<Preview1WorkerId>,
    worker_inputs: &WorkerInputRegistry,
    worker_slots: &mut usize,
) {
    let mut retained = VecDeque::with_capacity(actor_operations.len());
    for operation in std::mem::take(actor_operations) {
        match operation {
            ActorOperation::PrepareWorker { worker, reply, .. } => {
                preparing_workers.remove(&worker);
                *worker_slots = worker_slots.saturating_sub(1);
                remove_worker_input(worker_inputs, worker);
                if let Some(reply) = reply {
                    let _result = reply.send(Err(Preview1WorkerError::Cancelled));
                }
            }
            operation @ ActorOperation::ReleaseWorker { .. } => retained.push_back(operation),
            ActorOperation::Dispatch(_)
            | ActorOperation::Mailbox { .. }
            | ActorOperation::Call { .. } => {}
        }
    }
    *actor_operations = retained;
}

struct EpochTicker {
    stop: std_mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine, interval: Duration) -> Result<Self, Preview1WorkerError> {
        let (stop, stopped) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("brood-wasm-epoch".to_string())
            .spawn(move || {
                while stopped.recv_timeout(interval) == Err(std_mpsc::RecvTimeoutError::Timeout) {
                    engine.increment_epoch();
                }
            })
            .map_err(|error| Preview1WorkerError::ParentTask(error.to_string()))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        let _result = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _result = thread.join();
        }
    }
}
