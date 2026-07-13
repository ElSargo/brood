use std::{ops::Range, path::Path};

use anyhow::{Context, Result, ensure};
use wasmtime::{AsContext, AsContextMut, Config, Engine, Instance, Memory, Module, TypedFunc};

use crate::{
    mailbox::{ActorId, Mailbox, MailboxRegistry},
    policy_provider::{
        DEFAULT_PUBLICATION_SLOT, PublicationFrameLease, PublicationRegistry, PublicationSummary,
    },
};

pub const DEFAULT_GUEST_ARENA_ALIGN: usize = 8;
pub const DEFAULT_MAILBOX_ARENA_BYTES: usize = 64 * 1024;
pub const DEFAULT_EPOCH_TICKS: u64 = 1;
pub const EPOCH_RUN_CONTINUE: i64 = 0;
pub const EPOCH_RUN_STOP: i64 = -1;
pub const EPOCH_RUN_ERROR: i64 = -2;
pub const READ_MESSAGE_CLOSED: i32 = -1;
pub const READ_MESSAGE_ERROR: i32 = -2;
pub const MAX_PUBLICATION_SLOT_BYTES: usize = 128;
pub const MAX_PUBLICATION_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
pub struct Registry {
    mailboxes: MailboxRegistry,
    publications: PublicationRegistry,
}

#[derive(Clone)]
pub struct GuestArenaAllocator {
    memory: Memory,
    alloc: TypedFunc<(i32, i32), i32>,
    free: Option<TypedFunc<(i32, i32, i32), ()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestArena {
    ptr: i32,
    capacity: i32,
    align: i32,
    cursor: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestAllocation {
    ptr: i32,
    len: i32,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            mailboxes: MailboxRegistry::new(),
            publications: PublicationRegistry::new(),
        }
    }

    pub const fn mailboxes(&self) -> &MailboxRegistry {
        &self.mailboxes
    }

    pub const fn publications(&self) -> &PublicationRegistry {
        &self.publications
    }

    pub fn register_actor(&mut self, name: impl Into<String>, capacity: usize) -> Result<Mailbox> {
        self.mailboxes.register_actor(name, capacity)
    }

    pub fn register_host_mailbox(&mut self, capacity: usize) -> Result<Mailbox> {
        self.register_actor(crate::global_key_policy::HOST_ACTOR_NAME, capacity)
    }

    pub fn actor_id_for_name(&self, name: &str) -> Option<ActorId> {
        self.mailboxes.actor_id_for_name(name)
    }

    pub fn unregister_actor(&mut self, name: &str) -> bool {
        self.mailboxes.unregister_actor(name)
    }

    pub fn host_actor_id(&self) -> Option<ActorId> {
        self.actor_id_for_name(crate::global_key_policy::HOST_ACTOR_NAME)
    }

    pub fn try_post_named_message(&self, recipient: &str, payload: Vec<u8>) -> Result<()> {
        self.mailboxes.try_post_named_message(recipient, payload)
    }

    pub fn try_post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        self.mailboxes.try_post_message(recipient, payload)
    }

    pub fn try_post_host_message(&self, payload: Vec<u8>) -> Result<()> {
        self.try_post_named_message(crate::global_key_policy::HOST_ACTOR_NAME, payload)
    }

    pub async fn post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        self.mailboxes.post_message(recipient, payload).await
    }

    pub async fn post_host_message(&self, payload: Vec<u8>) -> Result<()> {
        let actor_id = self
            .host_actor_id()
            .context("host mailbox is not registered")?;
        self.post_message(actor_id, payload).await
    }

    pub fn try_post_message_from_range(
        &self,
        recipient: ActorId,
        memory: &[u8],
        payload: Range<usize>,
    ) -> Result<()> {
        self.mailboxes
            .try_post_message_from_range(recipient, memory, payload)
    }

    pub fn try_post_host_message_from_range(
        &self,
        memory: &[u8],
        payload: Range<usize>,
    ) -> Result<()> {
        let actor_id = self
            .host_actor_id()
            .context("host mailbox is not registered")?;
        self.try_post_message_from_range(actor_id, memory, payload)
    }

    pub fn append_default_publication_frame(
        &mut self,
        actor_id: ActorId,
        payload: Vec<u8>,
    ) -> Result<PublicationFrameLease> {
        self.append_publication_frame(actor_id, DEFAULT_PUBLICATION_SLOT, payload)
    }

    pub fn append_publication_frame(
        &mut self,
        actor_id: ActorId,
        slot: &str,
        payload: Vec<u8>,
    ) -> Result<PublicationFrameLease> {
        self.ensure_actor(actor_id)?;
        self.publications.append_frame(actor_id, slot, payload)
    }

    pub fn append_publication_frame_from_range(
        &mut self,
        actor_id: ActorId,
        slot: &str,
        memory: &[u8],
        payload: Range<usize>,
    ) -> Result<PublicationFrameLease> {
        self.ensure_actor(actor_id)?;
        self.publications
            .append_frame_from_range(actor_id, slot, memory, payload)
    }

    pub fn latest_default_publication_frame(
        &self,
        actor_id: ActorId,
    ) -> Result<Option<PublicationFrameLease>> {
        self.latest_publication_frame(actor_id, DEFAULT_PUBLICATION_SLOT)
    }

    pub fn latest_publication_frame(
        &self,
        actor_id: ActorId,
        slot: &str,
    ) -> Result<Option<PublicationFrameLease>> {
        self.ensure_actor(actor_id)?;
        ensure!(!slot.is_empty(), "publication slot must not be empty");
        Ok(self.publications.latest_frame(actor_id, slot))
    }

    pub fn publication_summaries(&self, actor_id: ActorId) -> Result<Vec<PublicationSummary>> {
        self.ensure_actor(actor_id)?;
        Ok(self.publications.summaries(actor_id))
    }

    fn ensure_actor(&self, actor_id: ActorId) -> Result<()> {
        ensure!(
            self.mailboxes.contains_actor(actor_id),
            "unknown actor id {}",
            actor_id.raw()
        );
        Ok(())
    }
}

impl GuestArenaAllocator {
    /// Load the guest allocator exports from a WASM instance.
    ///
    /// Guest modules that want host-created arenas should export:
    ///
    /// ```text
    /// memory
    /// manifold_arena_alloc(capacity: i32, align: i32) -> i32
    /// manifold_arena_free(ptr: i32, capacity: i32, align: i32) -> ()  // optional
    /// ```
    pub fn load<T: 'static>(
        store: impl AsContextMut<Data = T>,
        instance: &Instance,
    ) -> Result<Self> {
        crate::api_link::load_guest_arena_allocator(store, instance)
    }

    pub(crate) const fn from_exports(
        memory: Memory,
        alloc: TypedFunc<(i32, i32), i32>,
        free: Option<TypedFunc<(i32, i32, i32), ()>>,
    ) -> Self {
        Self {
            memory,
            alloc,
            free,
        }
    }

    pub const fn memory(&self) -> Memory {
        self.memory
    }

    pub fn allocate<T: 'static>(
        &self,
        store: impl AsContextMut<Data = T>,
        capacity: usize,
    ) -> Result<GuestArena> {
        self.allocate_aligned(store, capacity, DEFAULT_GUEST_ARENA_ALIGN)
    }

    pub async fn allocate_async<T: Send + 'static>(
        &self,
        store: impl AsContextMut<Data = T>,
        capacity: usize,
    ) -> Result<GuestArena> {
        self.allocate_aligned_async(store, capacity, DEFAULT_GUEST_ARENA_ALIGN)
            .await
    }

    pub fn allocate_aligned<T: 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        capacity: usize,
        align: usize,
    ) -> Result<GuestArena> {
        ensure!(capacity > 0, "guest arena capacity must be non-zero");
        ensure!(align > 0, "guest arena alignment must be non-zero");
        ensure!(
            align.is_power_of_two(),
            "guest arena alignment must be a power of two"
        );

        let capacity = i32::try_from(capacity).context("convert guest arena capacity")?;
        let align = i32::try_from(align).context("convert guest arena alignment")?;
        let ptr = self
            .alloc
            .call(&mut store, (capacity, align))
            .map_err(|err| {
                anyhow::anyhow!("call guest {}: {err}", crate::api_link::ARENA_ALLOC_EXPORT)
            })?;
        ensure_arena_pointer(ptr, align)?;

        let arena = GuestArena {
            ptr,
            capacity,
            align,
            cursor: 0,
        };
        self.ensure_arena_range(store.as_context(), &arena)?;
        Ok(arena)
    }

    pub async fn allocate_aligned_async<T: Send + 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        capacity: usize,
        align: usize,
    ) -> Result<GuestArena> {
        ensure!(capacity > 0, "guest arena capacity must be non-zero");
        ensure!(align > 0, "guest arena alignment must be non-zero");
        ensure!(
            align.is_power_of_two(),
            "guest arena alignment must be a power of two"
        );

        let capacity = i32::try_from(capacity).context("convert guest arena capacity")?;
        let align = i32::try_from(align).context("convert guest arena alignment")?;
        let ptr = self
            .alloc
            .call_async(&mut store, (capacity, align))
            .await
            .map_err(|err| {
                anyhow::anyhow!("call guest {}: {err}", crate::api_link::ARENA_ALLOC_EXPORT)
            })?;
        ensure_arena_pointer(ptr, align)?;

        let arena = GuestArena {
            ptr,
            capacity,
            align,
            cursor: 0,
        };
        self.ensure_arena_range(store.as_context(), &arena)?;
        Ok(arena)
    }

    pub fn free<T: 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        arena: GuestArena,
    ) -> Result<()> {
        if let Some(free) = &self.free {
            free.call(&mut store, (arena.ptr, arena.capacity, arena.align))
                .map_err(|err| {
                    anyhow::anyhow!("call guest {}: {err}", crate::api_link::ARENA_FREE_EXPORT)
                })?;
        }
        Ok(())
    }

    pub async fn free_async<T: Send + 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        arena: GuestArena,
    ) -> Result<()> {
        if let Some(free) = &self.free {
            free.call_async(&mut store, (arena.ptr, arena.capacity, arena.align))
                .await
                .map_err(|err| {
                    anyhow::anyhow!("call guest {}: {err}", crate::api_link::ARENA_FREE_EXPORT)
                })?;
        }
        Ok(())
    }

    pub fn write<T: 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        allocation: GuestAllocation,
        bytes: &[u8],
    ) -> Result<()> {
        let allocation_len =
            usize::try_from(allocation.len()).context("convert guest allocation length")?;
        ensure!(
            bytes.len() <= allocation_len,
            "guest allocation too small: need {} bytes but allocation has {allocation_len}",
            bytes.len()
        );
        self.memory
            .write(
                &mut store,
                usize::try_from(allocation.ptr()).context("convert guest allocation pointer")?,
                bytes,
            )
            .context("write guest arena allocation")
    }

    pub fn copy_frame<T: 'static>(
        &self,
        store: impl AsContextMut<Data = T>,
        arena: &mut GuestArena,
        frame: &PublicationFrameLease,
    ) -> Result<GuestAllocation> {
        self.copy_bytes(store, arena, frame.bytes(), DEFAULT_GUEST_ARENA_ALIGN)
    }

    pub fn copy_bytes<T: 'static>(
        &self,
        mut store: impl AsContextMut<Data = T>,
        arena: &mut GuestArena,
        bytes: &[u8],
        align: usize,
    ) -> Result<GuestAllocation> {
        let allocation = arena.allocate(bytes.len(), align)?;
        self.memory
            .write(
                &mut store,
                usize::try_from(allocation.ptr()).context("convert guest allocation pointer")?,
                bytes,
            )
            .context("write guest arena bytes")?;
        Ok(allocation)
    }

    fn ensure_arena_range<T: 'static>(
        &self,
        store: impl AsContext<Data = T>,
        arena: &GuestArena,
    ) -> Result<()> {
        let start = usize::try_from(arena.ptr).context("convert guest arena pointer")?;
        let capacity = usize::try_from(arena.capacity).context("convert guest arena capacity")?;
        let end = start
            .checked_add(capacity)
            .context("guest arena range overflowed")?;
        ensure!(
            end <= self.memory.data_size(store),
            "guest arena {start}..{end} is outside guest memory"
        );
        Ok(())
    }
}

impl GuestArena {
    pub const fn ptr(&self) -> i32 {
        self.ptr
    }

    pub const fn capacity(&self) -> i32 {
        self.capacity
    }

    pub const fn align(&self) -> i32 {
        self.align
    }

    pub fn remaining(&self) -> usize {
        usize::try_from(self.capacity).map_or(0, |capacity| capacity.saturating_sub(self.cursor))
    }

    pub fn allocate(&mut self, len: usize, align: usize) -> Result<GuestAllocation> {
        ensure!(align > 0, "guest allocation alignment must be non-zero");
        ensure!(
            align.is_power_of_two(),
            "guest allocation alignment must be a power of two"
        );

        let capacity = usize::try_from(self.capacity).context("convert guest arena capacity")?;
        let base = usize::try_from(self.ptr).context("convert guest arena pointer")?;
        let aligned = align_up(
            base.checked_add(self.cursor)
                .context("guest arena cursor overflowed")?,
            align,
        )?;
        let offset = aligned
            .checked_sub(base)
            .context("aligned guest allocation escaped arena")?;
        let end = offset
            .checked_add(len)
            .context("guest allocation range overflowed")?;
        ensure!(
            end <= capacity,
            "guest arena is too small: need {end} bytes but arena has {capacity}"
        );

        self.cursor = end;
        Ok(GuestAllocation {
            ptr: i32::try_from(aligned).context("convert guest allocation pointer")?,
            len: i32::try_from(len).context("convert guest allocation length")?,
        })
    }

    pub const fn reset(&mut self) {
        self.cursor = 0;
    }
}

impl GuestAllocation {
    pub const fn ptr(self) -> i32 {
        self.ptr
    }

    pub const fn len(self) -> i32 {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn range(self) -> Result<Range<usize>> {
        let start = usize::try_from(self.ptr).context("convert guest allocation pointer")?;
        let len = usize::try_from(self.len).context("convert guest allocation length")?;
        let end = start
            .checked_add(len)
            .context("guest allocation range overflowed")?;
        Ok(start..end)
    }
}

pub(crate) fn read_range<'a>(
    memory: &'a [u8],
    range: Range<usize>,
    label: &str,
) -> Result<&'a [u8]> {
    memory.get(range.clone()).with_context(|| {
        format!(
            "{label} range {}..{} is outside {} byte memory",
            range.start,
            range.end,
            memory.len()
        )
    })
}

pub(crate) fn async_epoch_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    Engine::new(&config).map_err(|err| anyhow::anyhow!("create async WASM engine: {err}"))
}

pub(crate) async fn load_module_async(engine: Engine, path: &Path) -> Result<Module> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        Module::from_file(&engine, &path)
            .map_err(|err| anyhow::anyhow!("load {}: {err}", path.display()))
    })
    .await
    .map_err(|err| anyhow::anyhow!("join WASM module loader: {err}"))?
}

fn ensure_arena_pointer(ptr: i32, align: i32) -> Result<()> {
    ensure!(ptr > 0, "guest arena allocator returned a null pointer");
    ensure!(
        usize::try_from(ptr).context("convert guest arena pointer")?
            % usize::try_from(align).context("convert guest arena alignment")?
            == 0,
        "guest arena allocator returned a misaligned pointer"
    );
    Ok(())
}

fn align_up(value: usize, align: usize) -> Result<usize> {
    debug_assert!(align > 0);
    debug_assert!(align.is_power_of_two());
    let mask = align - 1;
    value
        .checked_add(mask)
        .map(|value| value & !mask)
        .context("aligned value overflowed")
}
