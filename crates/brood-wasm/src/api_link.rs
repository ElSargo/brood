use anyhow::{Context, Result};
use wasmtime::{AsContextMut, Instance};

use crate::api::GuestArenaAllocator;

pub const HOST_IMPORT_MODULE: &str = "brood";
pub const GUEST_MEMORY_EXPORT: &str = "memory";
pub const ARENA_ALLOC_EXPORT: &str = "brood_arena_alloc";
pub const ARENA_FREE_EXPORT: &str = "brood_arena_free";
pub const READ_MESSAGE_IMPORT: &str = "read_message";
pub const EPOCH_RUN_EXPORT: &str = "brood_epoch_run";

pub fn load_guest_arena_allocator<T: 'static>(
    mut store: impl AsContextMut<Data = T>,
    instance: &Instance,
) -> Result<GuestArenaAllocator> {
    let memory = instance
        .get_memory(&mut store, GUEST_MEMORY_EXPORT)
        .context("guest must export memory")?;
    let alloc = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, ARENA_ALLOC_EXPORT)
        .map_err(|err| anyhow::anyhow!("guest must export {ARENA_ALLOC_EXPORT}: {err}"))?;
    let free = instance
        .get_typed_func::<(i32, i32, i32), ()>(&mut store, ARENA_FREE_EXPORT)
        .ok();

    Ok(GuestArenaAllocator::from_exports(memory, alloc, free))
}
