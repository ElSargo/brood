#![allow(
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    reason = "WASM host API scaffold favors stable public signatures before call sites are wired"
)]

pub mod actor;
mod actor_kernel;
mod actor_link;
pub mod api;
mod api_link;
pub mod completion_io;
pub mod component_actor;
pub mod computer_use_js;
pub mod coordination;
mod epoch_clock;
mod filesystem_watch;
pub mod global_key_policy;
mod global_key_policy_link;
pub mod host_message;
mod hosted_actor;
mod legacy_runtime;
mod lifecycle;
pub mod mailbox;
pub mod module;
mod module_link;
pub mod pointer_policy;
mod pointer_policy_link;
pub mod policy_provider;
mod policy_provider_link;
pub mod presentation;
pub mod preview1_worker;
pub mod resize;
pub mod resolved_layout;
mod resource_limits;
pub mod runtime_io;
mod test_harness;

pub use actor::{
    ACTOR_EMPTY_OUTBOX, ActorEpochOutcome, ActorOutbox, ActorOutboxMessage, ActorPublication,
    MAX_ACTOR_OUTBOX_BYTES, WasmActor, WasmActorConfig, pack_actor_outbox_slice,
    unpack_actor_outbox_slice,
};
pub use api::{
    DEFAULT_EPOCH_TICKS, DEFAULT_GUEST_ARENA_ALIGN, DEFAULT_MAILBOX_ARENA_BYTES,
    EPOCH_RUN_CONTINUE, EPOCH_RUN_ERROR, EPOCH_RUN_STOP, GuestAllocation, GuestArena,
    GuestArenaAllocator, MAX_PUBLICATION_FRAME_BYTES, MAX_PUBLICATION_SLOT_BYTES,
    READ_MESSAGE_CLOSED, READ_MESSAGE_ERROR, Registry,
};
pub use api_link::{
    ARENA_ALLOC_EXPORT, ARENA_FREE_EXPORT, EPOCH_RUN_EXPORT, GUEST_MEMORY_EXPORT,
    HOST_IMPORT_MODULE, READ_MESSAGE_IMPORT,
};
pub use completion_io::{
    ActorCompletion, ActorCompletionKind, CompletionIo, CompletionIoError, CompletionIoStats,
    FilesystemGrant, OperationId,
};
pub use component_actor::{
    ComponentActor, ComponentActorConfig, ComponentActorEvent, ComponentActorEventWaker,
    ComponentActorMailboxStats, ComponentActorPresentationEnvelopeSendError,
    ComponentActorSendError, ComponentAppEvent, ComponentChildEvent, PresentationScope, WorkerId,
};
pub use computer_use_js::{
    ComputerUseJsConfig, ComputerUseJsExecution, ComputerUseJsIsolate, ComputerUseJsMetrics,
    ComputerUseJsPlan,
};
pub use coordination::{
    ActorNotification, ActorWatchResult, CoordinationConfig, CoordinationHub, CoordinationStats,
    NotificationError, NotificationId, PollError, PollableId, ReadyPollable, WaitMode,
};
pub use filesystem_watch::{
    FilesystemWatchStats, NativeFilesystemWatcherService, NativeFilesystemWatcherStats,
};
pub use global_key_policy::{
    ABI_INPUT_MODE_NORMAL, ABI_INPUT_MODE_PASSTHROUGH, ABI_KEY_PRESSED, ABI_KEY_RELEASED,
    ABI_MOD_ALT, ABI_MOD_CTRL, ABI_MOD_SHIFT, DEFAULT_GLOBAL_KEY_POLICY_ARENA_BYTES,
    GlobalKeyActorMessage, GlobalKeyDecision, GlobalKeyPolicyInput, GlobalKeyPolicyInvocation,
    GlobalKeyPolicyOutcome, HOST_ACTOR_NAME, MAX_GLOBAL_KEY_POLICY_ARENA_BYTES,
    MAX_GLOBAL_KEY_POLICY_QUERY_BYTES, MAX_GLOBAL_KEY_POLICY_RESULT_BYTES, WasmGlobalKeyPolicy,
};
pub use global_key_policy_link::{
    GLOBAL_KEY_POLICY_RESULT_ABSTAIN, GLOBAL_KEY_POLICY_RESULT_ERROR, GLOBAL_KEY_POLICY_RESULT_OK,
};
pub use host_message::HostMessage;
pub use hosted_actor::{HostedWasmActor, aggregate_preview1_compat_stats};
pub use legacy_runtime::{LegacyWasmActorGeneration, LegacyWasmRuntimeEvent};
pub use lifecycle::{
    PluginLifecycleEntry, PluginLifecycleEvent, PluginStartContext, PluginStartReason,
    PluginStopContext, PluginStopReason,
};
pub use mailbox::{
    ActorId, MAX_MAILBOX_GENERATION_BYTES, MAX_MAILBOX_MESSAGE_BYTES, Mailbox, MailboxAddress,
    MailboxEnvelope, MailboxRegistry, PreparedMailboxRegistration,
};
pub use module::{
    DEFAULT_ACTOR_MAILBOX_CAPACITY, DEFAULT_ACTOR_NAME, LegacyWasmPluginArtifact,
    WasmActorDeclaration, WasmActorEventKind, WasmModuleInventory, WasmPluginArtifact,
    WasmPluginConfig, WasmPluginModule,
};
pub use module_link::{ACTOR_EXPORT_PREFIX, ACTOR_EXPORT_SUFFIX, ACTOR_SUBSCRIPTION_INFIX};
pub use pointer_policy::{
    DEFAULT_POINTER_POLICY_ARENA_BYTES, MAX_POINTER_POLICY_ACTOR_BYTES,
    MAX_POINTER_POLICY_ARENA_BYTES, MAX_POINTER_POLICY_MEMORY_BYTES,
    MAX_POINTER_POLICY_MESSAGE_BYTES, MAX_POINTER_POLICY_MESSAGES, MAX_POINTER_POLICY_QUERY_BYTES,
    MAX_POINTER_POLICY_RESULT_BYTES, MAX_POINTER_POLICY_TABLE_ELEMENTS,
    POINTER_POLICY_FUEL_PER_CALL, POINTER_POLICY_LEASE_HOLDER, PointerCapture, PointerDelivery,
    PointerModification, PointerPhase, PointerPolicyActorMessage, PointerPolicyInvocation,
    PointerPolicyQuery, PointerPolicyResult, WasmPointerPolicy,
};
pub use policy_provider::{
    DEFAULT_PUBLICATION_SLOT, POLICY_PROVIDER_RESPONSE_ABSTAIN, POLICY_PROVIDER_RESPONSE_ERROR,
    POLICY_PROVIDER_RESPONSE_OK, PolicyProviderDeclaration, PolicyProviderKind,
    PolicyProviderResponse, PublicationFrameLease, PublicationRegistry, PublicationSummary,
    WasmPolicyProvider,
};
pub use policy_provider_link::POLICY_PROVIDER_EXPORT_PREFIX;
pub use presentation::{
    ActorFrameDraw, FontIntent, FontStyle, FontWeight, PresentationError, PresentationEvent,
    PresentationHost, PurePaneOutput, PureRenderError, PureRenderRequest, PureRenderResult,
    RenderHint, RenderPaneTarget, RenderToken, SessionCommandCompletion, SessionCommandRequest,
    SessionEvent, SessionOperation, SessionSnapshot, SessionSummary, TextLayoutOptions,
    TextLayoutResult,
};
pub use preview1_worker::{
    COMPAT_DISPATCH_EXPORT, COMPAT_MARKER_EXPORT, COMPAT_MARKER_MAGIC, COMPAT_WORKER_EXPORT,
    Preview1ActorCapabilities, Preview1CompatActor, Preview1CompatStats, Preview1FilesystemGrant,
    Preview1Worker, Preview1WorkerConfig, Preview1WorkerError, Preview1WorkerId,
    Preview1WorkerRuntime, SHARED_MEMORY_IMPORT_MODULE, SHARED_MEMORY_IMPORT_NAME,
    WORKER_RELEASE_EXPORT, WORKER_RESERVE_EXPORT, declares_preview1_compat,
};
pub use resize::{
    FailLayoutRegion, InteractionId, LayoutRegionNotification, LayoutRegionRejectionReason,
    LayoutRegionResized, LayoutRegionResponseNotification, PresentationMode, ResizeEpoch,
    ReuseLayoutRegion, ReusePaneContent, SubmitLayoutRegion, SubmitSynchronizedLayout,
    SynchronizedLayoutAborted, SynchronizedLayoutAccepted, SynchronizedLayoutCompleted,
    SynchronizedLayoutDegraded, SynchronizedLayoutFragment, SynchronizedLayoutNotification,
    SynchronizedLayoutRejected, SynchronizedLayoutRejectionReason, SynchronizedLayoutSuperseded,
};
pub use resolved_layout::{
    CursorRole, DividerAxis, DividerRole, InputBehavior, InteractionZone, LayoutChildSlot,
    LayoutDivider, LayoutItemId, LayoutItemKind, LayoutLeaseToken, LayoutSize, LocalPoint,
    LocalTransform, PointerEventMask, ResolvedLayout, ResolvedLayoutItem, ZoneInputBehavior,
};
pub use resource_limits::WasmResourceLimits;
pub use runtime_io::{
    ByteStream, ChildHandle, ChildStatus, ProcessIo, PtyIo, PtySpawnOptions, RuntimeIoCapabilities,
    RuntimeIoConfig, RuntimeIoError, RuntimeIoHub, RuntimeIoStats, StreamRead, TerminalProfile,
    TimerHandle, TimerStatus,
};
pub use test_harness::{
    TestHarnessChildId, TestHarnessCleanupResult, TestHarnessCompletion, TestHarnessOutcomeKind,
    TestHarnessRequest, TestHarnessRuntime, TestHarnessSignal,
};
