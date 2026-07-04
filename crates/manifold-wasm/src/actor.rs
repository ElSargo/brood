use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Module, Store};

use crate::{
    actor_link::{EpochRunFunc, load_epoch_run_export, new_actor_linker},
    api::{
        DEFAULT_EPOCH_TICKS, DEFAULT_GUEST_ARENA_ALIGN, DEFAULT_MAILBOX_ARENA_BYTES,
        EPOCH_RUN_CONTINUE, EPOCH_RUN_ERROR, EPOCH_RUN_STOP, GuestAllocation, GuestArena,
        GuestArenaAllocator, Registry, async_epoch_engine, load_module_async,
    },
    api_link::EPOCH_RUN_EXPORT,
    mailbox::Mailbox,
};

pub const ACTOR_EMPTY_OUTBOX: i64 = EPOCH_RUN_CONTINUE;
pub const MAX_ACTOR_OUTBOX_BYTES: usize = 1024 * 1024;
const OUTBOX_PTR_BITS: u32 = 32;
const OUTBOX_LEN_MASK: i64 = 0xffff_ffff;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorOutbox {
    #[serde(default)]
    pub messages: Vec<ActorOutboxMessage>,
    #[serde(default)]
    pub publications: Vec<ActorPublication>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorOutboxMessage {
    pub actor: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorPublication {
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorEpochOutcome {
    Continue { outbox: Option<ActorOutbox> },
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmActorConfig {
    pub inbox_arena_bytes: usize,
    pub inbox_arena_align: usize,
    pub epoch_ticks: u64,
}

pub struct WasmActor {
    engine: wasmtime::Engine,
    store: Store<WasmActorStore>,
    allocator: GuestArenaAllocator,
    run_epoch: EpochRunFunc,
    epoch_ticks: u64,
}

#[derive(Debug)]
pub(crate) struct WasmActorStore {
    pub(crate) mailbox: Mailbox,
    pub(crate) inbox_arena: Option<GuestArena>,
    pub(crate) last_message: Option<GuestAllocation>,
    current_epoch: u64,
}

impl Default for WasmActorConfig {
    fn default() -> Self {
        Self {
            inbox_arena_bytes: DEFAULT_MAILBOX_ARENA_BYTES,
            inbox_arena_align: DEFAULT_GUEST_ARENA_ALIGN,
            epoch_ticks: DEFAULT_EPOCH_TICKS,
        }
    }
}

impl WasmActor {
    pub async fn load(path: &Path, mailbox: Mailbox) -> Result<Self> {
        Self::load_with_config(path, mailbox, WasmActorConfig::default()).await
    }

    pub async fn load_with_config(
        path: &Path,
        mailbox: Mailbox,
        config: WasmActorConfig,
    ) -> Result<Self> {
        ensure!(config.epoch_ticks > 0, "epoch ticks must be non-zero");

        let engine = async_epoch_engine()?;
        let module = load_module_async(engine.clone(), path).await?;
        Self::load_from_module(engine, &module, mailbox, config, EPOCH_RUN_EXPORT).await
    }

    pub(crate) async fn load_from_module(
        engine: Engine,
        module: &Module,
        mailbox: Mailbox,
        config: WasmActorConfig,
        run_export: &str,
    ) -> Result<Self> {
        ensure!(config.epoch_ticks > 0, "epoch ticks must be non-zero");

        let linker = new_actor_linker(&engine)?;

        let mut store = Store::new(&engine, WasmActorStore::new(mailbox));
        store.set_epoch_deadline(config.epoch_ticks);
        store.epoch_deadline_async_yield_and_update(config.epoch_ticks);

        let instance = linker
            .instantiate_async(&mut store, module)
            .await
            .map_err(|err| anyhow::anyhow!("instantiate WASM actor module: {err}"))?;
        let allocator = GuestArenaAllocator::load(&mut store, &instance)?;
        let inbox_arena = allocator
            .allocate_aligned_async(
                &mut store,
                config.inbox_arena_bytes,
                config.inbox_arena_align,
            )
            .await?;
        store.data_mut().inbox_arena = Some(inbox_arena);

        let run_epoch = load_epoch_run_export(&instance, &mut store, run_export)?;

        Ok(Self {
            engine,
            store,
            allocator,
            run_epoch,
            epoch_ticks: config.epoch_ticks,
        })
    }

    pub async fn run_epoch_loop(&mut self, registry: &mut Registry) -> Result<ActorEpochOutcome> {
        loop {
            let outcome = self.run_one_epoch_and_dispatch(registry).await?;
            if !matches!(outcome, ActorEpochOutcome::Continue { .. }) {
                return Ok(outcome);
            }
        }
    }

    pub async fn run_one_epoch(&mut self) -> Result<ActorEpochOutcome> {
        self.engine.increment_epoch();
        self.store
            .epoch_deadline_async_yield_and_update(self.epoch_ticks);
        let (epoch, inbox_ptr, inbox_cap) = self.next_epoch_context()?;
        let raw = self
            .run_epoch
            .call_async(&mut self.store, (epoch, inbox_ptr, inbox_cap))
            .await
            .map_err(|err| anyhow::anyhow!("call guest {EPOCH_RUN_EXPORT}: {err}"))?;
        self.decode_epoch_result(raw)
    }

    pub async fn run_one_epoch_and_dispatch(
        &mut self,
        registry: &mut Registry,
    ) -> Result<ActorEpochOutcome> {
        let outcome = self.run_one_epoch().await?;
        if let ActorEpochOutcome::Continue {
            outbox: Some(outbox),
        } = &outcome
        {
            self.dispatch_outbox(registry, outbox)?;
        }
        Ok(outcome)
    }

    pub fn dispatch_outbox(&self, registry: &mut Registry, outbox: &ActorOutbox) -> Result<()> {
        let actor_id = self.store.data().mailbox.actor_id();
        for publication in &outbox.publications {
            registry.append_default_publication_frame(actor_id, publication.payload.clone())?;
        }
        for message in &outbox.messages {
            registry
                .try_post_named_message(&message.actor, message.payload.clone())
                .with_context(|| format!("dispatch actor outbox message to {:?}", message.actor))?;
        }
        Ok(())
    }

    pub fn actor_id(&self) -> crate::ActorId {
        self.store.data().mailbox.actor_id()
    }

    pub fn inbox_arena(&self) -> Option<&GuestArena> {
        self.store.data().inbox_arena.as_ref()
    }

    pub fn last_message(&self) -> Option<GuestAllocation> {
        self.store.data().last_message
    }

    pub async fn free_inbox_arena(&mut self) -> Result<()> {
        let arena = self.store.data_mut().inbox_arena.take();
        if let Some(arena) = arena {
            self.allocator.free_async(&mut self.store, arena).await?;
        }
        Ok(())
    }

    fn next_epoch_context(&mut self) -> Result<(i64, i32, i32)> {
        let data = self.store.data_mut();
        data.current_epoch = data
            .current_epoch
            .checked_add(1)
            .context("WASM actor epoch counter overflowed")?;
        let arena = data
            .inbox_arena
            .as_ref()
            .context("inbox arena has not been allocated")?;
        let epoch = i64::try_from(data.current_epoch).context("convert WASM actor epoch")?;
        Ok((epoch, arena.ptr(), arena.capacity()))
    }

    fn decode_epoch_result(&self, raw: i64) -> Result<ActorEpochOutcome> {
        match raw {
            EPOCH_RUN_CONTINUE => Ok(ActorEpochOutcome::Continue { outbox: None }),
            EPOCH_RUN_STOP => Ok(ActorEpochOutcome::Stop),
            EPOCH_RUN_ERROR => anyhow::bail!("WASM actor returned error status"),
            status if status > 0 => {
                let (ptr, len) = unpack_actor_outbox_slice(status)?;
                let outbox = self.read_actor_outbox(ptr, len)?;
                Ok(ActorEpochOutcome::Continue {
                    outbox: Some(outbox),
                })
            }
            status => anyhow::bail!("WASM actor returned unknown status {status}"),
        }
    }

    fn read_actor_outbox(&self, ptr: i32, len: i32) -> Result<ActorOutbox> {
        ensure!(ptr >= 0, "actor outbox pointer must be non-negative");
        ensure!(len >= 0, "actor outbox length must be non-negative");
        let len = usize::try_from(len).context("convert actor outbox length")?;
        ensure!(
            len <= MAX_ACTOR_OUTBOX_BYTES,
            "actor outbox exceeds {MAX_ACTOR_OUTBOX_BYTES} byte limit"
        );
        let start = usize::try_from(ptr).context("convert actor outbox pointer")?;
        let end = start
            .checked_add(len)
            .context("actor outbox range overflowed")?;
        let memory = self.allocator.memory().data(&self.store);
        let bytes = memory.get(start..end).with_context(|| {
            format!("actor outbox range {start}..{end} is outside guest memory")
        })?;
        serde_json::from_slice(bytes).context("decode actor outbox")
    }
}

impl WasmActorStore {
    const fn new(mailbox: Mailbox) -> Self {
        Self {
            mailbox,
            inbox_arena: None,
            last_message: None,
            current_epoch: 0,
        }
    }
}

pub fn pack_actor_outbox_slice(ptr: i32, len: i32) -> Result<i64> {
    ensure!(ptr > 0, "actor outbox pointer must be positive");
    ensure!(len > 0, "actor outbox length must be positive");
    let ptr = i64::from(ptr);
    let len = i64::from(len);
    ensure!(
        ptr <= i64::from(i32::MAX),
        "actor outbox pointer is too large"
    );
    ensure!(
        len <= i64::from(i32::MAX),
        "actor outbox length is too large"
    );
    Ok((ptr << OUTBOX_PTR_BITS) | len)
}

pub fn unpack_actor_outbox_slice(value: i64) -> Result<(i32, i32)> {
    ensure!(value > 0, "packed actor outbox slice must be positive");
    let ptr = i32::try_from(value >> OUTBOX_PTR_BITS).context("unpack actor outbox pointer")?;
    let len = i32::try_from(value & OUTBOX_LEN_MASK).context("unpack actor outbox length")?;
    ensure!(ptr > 0, "actor outbox pointer must be positive");
    ensure!(len > 0, "actor outbox length must be positive");
    Ok((ptr, len))
}
