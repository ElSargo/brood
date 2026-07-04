use anyhow::{Context, Result, ensure};
use wasmtime::{Caller, Engine, Instance, Linker, Memory, Store, TypedFunc};

use crate::{
    actor::{PendingPublicationFrame, WasmActorStore},
    api::{
        DEFAULT_GUEST_ARENA_ALIGN, MAX_PUBLICATION_FRAME_BYTES, MAX_PUBLICATION_SLOT_BYTES,
        PUBLISH_ERROR, PUBLISH_OK, READ_MESSAGE_CLOSED, READ_MESSAGE_ERROR,
    },
    api_link::{GUEST_MEMORY_EXPORT, HOST_IMPORT_MODULE, PUBLISH_IMPORT, READ_MESSAGE_IMPORT},
    policy_provider::DEFAULT_PUBLICATION_SLOT,
};

pub type EpochRunFunc = TypedFunc<(i64, i32, i32), i32>;

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
        .get_typed_func::<(i64, i32, i32), i32>(store, run_export)
        .map_err(|err| {
            anyhow::anyhow!(
                "guest must export {run_export}(epoch: i64, mailbox_ptr: i32, mailbox_cap: i32) -> i32: {err}"
            )
        })
}

fn link_actor_imports(linker: &mut Linker<WasmActorStore>) -> Result<()> {
    link_read_message(linker)?;
    link_publish(linker)?;
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

fn link_publish(linker: &mut Linker<WasmActorStore>) -> Result<()> {
    linker
        .func_wrap_async(
            HOST_IMPORT_MODULE,
            PUBLISH_IMPORT,
            |mut caller: Caller<'_, WasmActorStore>,
             (slot_ptr, slot_len, payload_ptr, payload_len): (i32, i32, i32, i32)| {
                Box::new(async move {
                    match queue_publication_from_guest(
                        &mut caller,
                        slot_ptr,
                        slot_len,
                        payload_ptr,
                        payload_len,
                    ) {
                        Ok(()) => PUBLISH_OK,
                        Err(err) => {
                            tracing::warn!(?err, "WASM actor publish import failed");
                            PUBLISH_ERROR
                        }
                    }
                })
            },
        )
        .map_err(|err| anyhow::anyhow!("define {HOST_IMPORT_MODULE}.{PUBLISH_IMPORT}: {err}"))?;
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
            .mailbox_arena
            .as_mut()
            .context("mailbox arena has not been allocated")?;
        arena.reset();
        arena.allocate(message.len(), DEFAULT_GUEST_ARENA_ALIGN)?
    };

    let memory = guest_memory(&mut caller)?;
    memory
        .write(
            &mut caller,
            usize::try_from(allocation.ptr()).context("convert guest mailbox pointer")?,
            &message,
        )
        .context("write mailbox message into guest arena")?;
    caller.data_mut().last_message = Some(allocation);
    i32::try_from(message.len()).context("convert mailbox message length")
}

fn queue_publication_from_guest(
    caller: &mut Caller<'_, WasmActorStore>,
    slot_ptr: i32,
    slot_len: i32,
    payload_ptr: i32,
    payload_len: i32,
) -> Result<()> {
    let actor_id = caller.data().mailbox.actor_id();
    let slot = read_guest_publication_slot(caller, slot_ptr, slot_len)?;
    let payload = read_guest_bytes(
        caller,
        payload_ptr,
        payload_len,
        MAX_PUBLICATION_FRAME_BYTES,
        "publication payload",
    )?;
    caller
        .data_mut()
        .pending_publications
        .push(PendingPublicationFrame {
            actor_id,
            slot,
            payload,
        });
    Ok(())
}

fn read_guest_publication_slot(
    caller: &mut Caller<'_, WasmActorStore>,
    ptr: i32,
    len: i32,
) -> Result<String> {
    if len == 0 {
        return Ok(DEFAULT_PUBLICATION_SLOT.to_string());
    }

    let bytes = read_guest_bytes(
        caller,
        ptr,
        len,
        MAX_PUBLICATION_SLOT_BYTES,
        "publication slot",
    )?;
    let slot = String::from_utf8(bytes).context("publication slot must be UTF-8")?;
    ensure!(!slot.is_empty(), "publication slot must not be empty");
    Ok(slot)
}

fn read_guest_bytes(
    caller: &mut Caller<'_, WasmActorStore>,
    ptr: i32,
    len: i32,
    max_len: usize,
    label: &str,
) -> Result<Vec<u8>> {
    ensure!(ptr >= 0, "guest {label} pointer must be non-negative");
    ensure!(len >= 0, "guest {label} length must be non-negative");
    let len = usize::try_from(len).context("convert guest byte length")?;
    ensure!(len <= max_len, "guest {label} exceeds {max_len} byte limit");
    let ptr = usize::try_from(ptr).context("convert guest byte pointer")?;
    let memory = guest_memory(caller)?;
    let mut bytes = vec![0; len];
    memory
        .read(caller, ptr, &mut bytes)
        .with_context(|| format!("read guest {label}"))?;
    Ok(bytes)
}

fn guest_memory(caller: &mut Caller<'_, WasmActorStore>) -> Result<Memory> {
    caller
        .get_export(GUEST_MEMORY_EXPORT)
        .and_then(wasmtime::Extern::into_memory)
        .context("guest must export memory")
}
