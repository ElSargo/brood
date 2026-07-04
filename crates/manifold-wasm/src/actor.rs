use std::path::Path;

use anyhow::{Context, Result, ensure};
use wasmtime::{Engine, Module, Store};

use crate::{
    actor_link::{EpochRunFunc, load_epoch_run_export, new_actor_linker},
    api::{
        DEFAULT_EPOCH_TICKS, DEFAULT_GUEST_ARENA_ALIGN, DEFAULT_MAILBOX_ARENA_BYTES,
        EPOCH_RUN_CONTINUE, GuestAllocation, GuestArena, GuestArenaAllocator, Registry,
        async_epoch_engine, load_module_async,
    },
    api_link::EPOCH_RUN_EXPORT,
    mailbox::{ActorId, Mailbox},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPublicationFrame {
    pub actor_id: ActorId,
    pub slot: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmActorConfig {
    pub mailbox_arena_bytes: usize,
    pub mailbox_arena_align: usize,
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
    pub(crate) mailbox_arena: Option<GuestArena>,
    pub(crate) last_message: Option<GuestAllocation>,
    pub(crate) pending_publications: Vec<PendingPublicationFrame>,
    current_epoch: u64,
}

impl Default for WasmActorConfig {
    fn default() -> Self {
        Self {
            mailbox_arena_bytes: DEFAULT_MAILBOX_ARENA_BYTES,
            mailbox_arena_align: DEFAULT_GUEST_ARENA_ALIGN,
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
        let mailbox_arena = allocator
            .allocate_aligned_async(
                &mut store,
                config.mailbox_arena_bytes,
                config.mailbox_arena_align,
            )
            .await?;
        store.data_mut().mailbox_arena = Some(mailbox_arena);

        let run_epoch = load_epoch_run_export(&instance, &mut store, run_export)?;

        Ok(Self {
            engine,
            store,
            allocator,
            run_epoch,
            epoch_ticks: config.epoch_ticks,
        })
    }

    pub async fn run_epoch_loop(&mut self) -> Result<i32> {
        loop {
            let status = self.run_one_epoch().await?;
            if status != EPOCH_RUN_CONTINUE {
                return Ok(status);
            }
        }
    }

    pub async fn run_epoch_loop_and_publish(&mut self, registry: &mut Registry) -> Result<i32> {
        loop {
            let status = self.run_one_epoch_and_publish(registry).await?;
            if status != EPOCH_RUN_CONTINUE {
                return Ok(status);
            }
        }
    }

    pub async fn run_one_epoch(&mut self) -> Result<i32> {
        self.engine.increment_epoch();
        self.store
            .epoch_deadline_async_yield_and_update(self.epoch_ticks);
        let (epoch, mailbox_ptr, mailbox_cap) = self.next_epoch_context()?;
        self.run_epoch
            .call_async(&mut self.store, (epoch, mailbox_ptr, mailbox_cap))
            .await
            .map_err(|err| anyhow::anyhow!("call guest {EPOCH_RUN_EXPORT}: {err}"))
    }

    pub async fn run_one_epoch_and_publish(&mut self, registry: &mut Registry) -> Result<i32> {
        let status = self.run_one_epoch().await?;
        let _published = self.drain_pending_publications(registry)?;
        Ok(status)
    }

    pub fn drain_pending_publications(&mut self, registry: &mut Registry) -> Result<usize> {
        let frames = self
            .store
            .data_mut()
            .pending_publications
            .drain(..)
            .collect::<Vec<_>>();
        let published = frames.len();
        for frame in frames {
            registry.append_publication_frame(frame.actor_id, &frame.slot, frame.payload)?;
        }
        Ok(published)
    }

    pub fn take_pending_publications(&mut self) -> Vec<PendingPublicationFrame> {
        self.store
            .data_mut()
            .pending_publications
            .drain(..)
            .collect()
    }

    pub fn mailbox_arena(&self) -> Option<&GuestArena> {
        self.store.data().mailbox_arena.as_ref()
    }

    pub fn last_message(&self) -> Option<GuestAllocation> {
        self.store.data().last_message
    }

    pub async fn free_mailbox_arena(&mut self) -> Result<()> {
        let arena = self.store.data_mut().mailbox_arena.take();
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
            .mailbox_arena
            .as_ref()
            .context("mailbox arena has not been allocated")?;
        let epoch = i64::try_from(data.current_epoch).context("convert WASM actor epoch")?;
        Ok((epoch, arena.ptr(), arena.capacity()))
    }
}

impl WasmActorStore {
    const fn new(mailbox: Mailbox) -> Self {
        Self {
            mailbox,
            mailbox_arena: None,
            last_message: None,
            pending_publications: Vec::new(),
            current_epoch: 0,
        }
    }
}
