use anyhow::{Context, Result};
use wasmtime::{Caller, Engine, Instance, Linker, Store, TypedFunc};

use crate::{
    actor::WasmActorStore,
    api::{DEFAULT_GUEST_ARENA_ALIGN, READ_MESSAGE_CLOSED, READ_MESSAGE_ERROR},
    api_link::{GUEST_MEMORY_EXPORT, HOST_IMPORT_MODULE, READ_MESSAGE_IMPORT},
};

pub type EpochRunFunc = TypedFunc<(i64, i32, i32), i64>;

pub fn new_actor_linker(engine: &Engine) -> Result<Linker<WasmActorStore>> {
    let mut linker = Linker::new(engine);
    link_actor_imports(&mut linker)?;
    Ok(linker)
}

pub fn load_epoch_run_export(
    instance: &Instance,
    store: &mut Store<WasmActorStore>,
    run_export: &str,
) -> Result<EpochRunFunc> {
    instance
        .get_typed_func::<(i64, i32, i32), i64>(store, run_export)
        .map_err(|err| {
            anyhow::anyhow!(
                "guest must export {run_export}(epoch: i64, inbox_ptr: i32, inbox_cap: i32) -> i64: {err}"
            )
        })
}

fn link_actor_imports(linker: &mut Linker<WasmActorStore>) -> Result<()> {
    link_read_message(linker)?;
    Ok(())
}

fn link_read_message(linker: &mut Linker<WasmActorStore>) -> Result<()> {
    linker
        .func_wrap_async(HOST_IMPORT_MODULE, READ_MESSAGE_IMPORT, |caller, ()| {
            Box::new(async move {
                match read_message_into_guest_arena(caller).await {
                    Ok(len) => len,
                    Err(err) => {
                        tracing::warn!(?err, "WASM actor read_message import failed");
                        READ_MESSAGE_ERROR
                    }
                }
            })
        })
        .map_err(|err| {
            anyhow::anyhow!("define {HOST_IMPORT_MODULE}.{READ_MESSAGE_IMPORT}: {err}")
        })?;
    Ok(())
}

async fn read_message_into_guest_arena(mut caller: Caller<'_, WasmActorStore>) -> Result<i32> {
    let message = match caller.data_mut().mailbox.read_message().await {
        Ok(message) => message,
        Err(err) => {
            tracing::warn!(?err, "WASM actor mailbox closed while reading message");
            return Ok(READ_MESSAGE_CLOSED);
        }
    };

    let allocation = {
        let data = caller.data_mut();
        let arena = data
            .inbox_arena
            .as_mut()
            .context("inbox arena has not been allocated")?;
        arena.reset();
        arena.allocate(message.len(), DEFAULT_GUEST_ARENA_ALIGN)?
    };

    let memory = caller
        .get_export(GUEST_MEMORY_EXPORT)
        .and_then(wasmtime::Extern::into_memory)
        .context("guest must export memory")?;
    memory
        .write(
            &mut caller,
            usize::try_from(allocation.ptr()).context("convert guest inbox pointer")?,
            &message,
        )
        .context("write mailbox message into guest inbox arena")?;
    caller.data_mut().last_message = Some(allocation);
    i32::try_from(message.len()).context("convert mailbox message length")
}
