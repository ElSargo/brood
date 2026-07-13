#![allow(
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    reason = "WASM host API scaffold favors stable public signatures before call sites are wired"
)]

pub mod actor;
mod actor_link;
pub mod api;
mod api_link;
pub mod completion_io;
pub mod component_actor;
pub mod coordination;
mod filesystem_watch;
pub mod global_key_policy;
mod global_key_policy_link;
pub mod layout_policy;
mod lifecycle;
pub mod mailbox;
pub mod module;
mod module_link;
pub mod policy_provider;
mod policy_provider_link;
pub mod preview1_worker;
mod resource_limits;
pub mod runtime_io;

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
    ComponentActor, ComponentActorConfig, ComponentActorEvent, ComponentActorMailboxStats, WorkerId,
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
    GlobalKeyActorMessage, GlobalKeyDecision, GlobalKeyPolicyInput,
    GlobalKeyPolicyInvocationBuffers, GlobalKeyPolicyOutcome, HOST_ACTOR_NAME, HostMessage,
    MAX_GLOBAL_KEY_POLICY_ARENA_BYTES, MAX_GLOBAL_KEY_POLICY_QUERY_BYTES,
    MAX_GLOBAL_KEY_POLICY_RESULT_BYTES, WasmGlobalKeyPolicy, host_message,
};
pub use global_key_policy_link::{
    GLOBAL_KEY_POLICY_RESULT_ABSTAIN, GLOBAL_KEY_POLICY_RESULT_ERROR, GLOBAL_KEY_POLICY_RESULT_OK,
};
pub use layout_policy::{
    InputBehavior, LayoutItemId, LayoutItemKind, LayoutSize, LocalPoint, LocalTransform,
    ResolvedLayout, ResolvedLayoutItem,
};
pub use lifecycle::{
    PluginLifecycleEntry, PluginLifecycleEvent, PluginStartContext, PluginStartReason,
    PluginStopContext, PluginStopReason,
};
pub use mailbox::{
    ActorId, MAX_MAILBOX_GENERATION_BYTES, MAX_MAILBOX_MESSAGE_BYTES, Mailbox, MailboxRegistry,
};
pub use module::{
    DEFAULT_ACTOR_MAILBOX_CAPACITY, DEFAULT_ACTOR_NAME, WasmActorDeclaration, WasmModuleInventory,
    WasmPluginConfig, WasmPluginModule,
};
pub use module_link::{ACTOR_EXPORT_PREFIX, ACTOR_EXPORT_SUFFIX};
pub use policy_provider::{
    DEFAULT_PUBLICATION_SLOT, POLICY_PROVIDER_RESPONSE_ABSTAIN, POLICY_PROVIDER_RESPONSE_ERROR,
    POLICY_PROVIDER_RESPONSE_OK, PolicyProviderDeclaration, PolicyProviderKind,
    PolicyProviderResponse, PublicationFrameLease, PublicationRegistry, PublicationSummary,
    WasmPolicyProvider,
};
pub use policy_provider_link::POLICY_PROVIDER_EXPORT_PREFIX;
pub use preview1_worker::{
    COMPAT_DISPATCH_EXPORT, COMPAT_MARKER_EXPORT, COMPAT_MARKER_MAGIC, COMPAT_WORKER_EXPORT,
    Preview1ActorCapabilities, Preview1CompatActor, Preview1CompatStats, Preview1FilesystemGrant,
    Preview1Worker, Preview1WorkerConfig, Preview1WorkerError, Preview1WorkerId,
    Preview1WorkerRuntime, SHARED_MEMORY_IMPORT_MODULE, SHARED_MEMORY_IMPORT_NAME,
    WORKER_RELEASE_EXPORT, WORKER_RESERVE_EXPORT, declares_preview1_compat,
};
pub use resource_limits::WasmResourceLimits;
pub use runtime_io::{
    ByteStream, ChildHandle, ChildStatus, ProcessIo, PtyIo, RuntimeIoCapabilities, RuntimeIoConfig,
    RuntimeIoError, RuntimeIoHub, StreamRead, TimerHandle, TimerStatus,
};

#[cfg(test)]
mod tests;
