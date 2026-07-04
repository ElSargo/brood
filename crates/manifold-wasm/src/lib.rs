#![allow(
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    reason = "WASM host API scaffold favors stable public signatures before call sites are wired"
)]

pub mod actor;
mod actor_link;
pub mod api;
mod api_link;
pub mod mailbox;
pub mod module;
mod module_link;
pub mod policy_provider;
mod policy_provider_link;

pub use actor::{PendingPublicationFrame, WasmActor, WasmActorConfig};
pub use api::{
    DEFAULT_EPOCH_TICKS, DEFAULT_GUEST_ARENA_ALIGN, DEFAULT_MAILBOX_ARENA_BYTES,
    EPOCH_RUN_CONTINUE, EPOCH_RUN_STOP, GuestAllocation, GuestArena, GuestArenaAllocator,
    MAX_PUBLICATION_FRAME_BYTES, MAX_PUBLICATION_SLOT_BYTES, PUBLISH_ERROR, PUBLISH_OK,
    READ_MESSAGE_CLOSED, READ_MESSAGE_ERROR, Registry,
};
pub use api_link::{
    ARENA_ALLOC_EXPORT, ARENA_FREE_EXPORT, EPOCH_RUN_EXPORT, GUEST_MEMORY_EXPORT,
    HOST_IMPORT_MODULE, PUBLISH_IMPORT, READ_MESSAGE_IMPORT,
};
pub use mailbox::{ActorId, Mailbox, MailboxRegistry};
pub use module::{
    DEFAULT_ACTOR_MAILBOX_CAPACITY, DEFAULT_ACTOR_NAME, WasmActorDeclaration, WasmModuleInventory,
    WasmPluginConfig, WasmPluginModule,
};
pub use module_link::{ACTOR_EXPORT_PREFIX, ACTOR_EXPORT_SUFFIX};
pub use policy_provider::{
    DEFAULT_PUBLICATION_SLOT, PolicyProviderDeclaration, PolicyProviderKind, PublicationFrameLease,
    PublicationRegistry, PublicationSummary, WasmPolicyProvider,
};
pub use policy_provider_link::POLICY_PROVIDER_EXPORT_PREFIX;
