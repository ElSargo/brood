use std::{alloc::Layout, slice};

const OUTBOX_CAPACITY: usize = 64 * 1024;
static mut OUTBOX: [u8; OUTBOX_CAPACITY] = [0; OUTBOX_CAPACITY];

#[link(wasm_import_module = "brood")]
unsafe extern "C" {
    fn read_message() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_arena_alloc(capacity: i32, align: i32) -> i32 {
    let Ok(layout) = arena_layout(capacity, align) else {
        return 0;
    };
    i32::try_from(unsafe { std::alloc::alloc(layout) }.addr()).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_arena_free(ptr: i32, capacity: i32, align: i32) {
    let Ok(layout) = arena_layout(capacity, align) else {
        return;
    };
    if ptr > 0 {
        unsafe { std::alloc::dealloc(ptr as usize as *mut u8, layout) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_actor_layout_subscribe_pane_mapped() {}

#[unsafe(no_mangle)]
pub extern "C" fn brood_actor_layout_subscribe_layout_state() {}

#[unsafe(no_mangle)]
pub extern "C" fn brood_actor_layout_epoch_run(
    _epoch: i64,
    inbox_ptr: i32,
    _inbox_cap: i32,
) -> i64 {
    let message_len = unsafe { read_message() };
    let Some(message) = guest_message(inbox_ptr, message_len) else {
        return 0;
    };
    let Ok(event) = serde_json::from_slice::<serde_json::Value>(message) else {
        return -2;
    };
    if event.get("type").and_then(serde_json::Value::as_str) != Some("layout-state") {
        return 0;
    }
    let panes = event
        .get("visible_panes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_u64)
        .take(2)
        .collect::<Vec<_>>();
    if panes.len() != 2 {
        return 0;
    }

    #[cfg(not(feature = "replacement"))]
    let transforms = [(-1.0, -1.0, 1.0, 2.0), (0.0, -1.0, 1.0, 2.0)];
    #[cfg(feature = "replacement")]
    let transforms = [(-1.0, -1.0, 2.0, 1.0), (-1.0, 0.0, 2.0, 1.0)];

    let items = panes
        .iter()
        .zip(transforms)
        .enumerate()
        .map(|(index, (pane, (u, v, w, h)))| {
            serde_json::json!({
                "id": 1000 + index,
                "kind": {"type": "pane", "pane": pane},
                "z": index as f32,
                "transform": {"offset": {"u": u, "v": v}, "size": {"u": w, "v": h}},
                "input": "pane"
            })
        })
        .collect::<Vec<_>>();
    let command = serde_json::json!({
        "type": "set-resolved-layout",
        "lease": event.get("lease").cloned().unwrap_or(serde_json::Value::Null),
        "revision": event.get("revision").cloned().unwrap_or(serde_json::Value::Null),
        "layout": {"items": items},
        "focus": panes[1]
    });
    actor_outbox(command)
}

/// This real actor intentionally has no event-subscription marker. If the host
/// regresses to broadcast routing, each pane event makes the proof visibly grow
/// an unwanted third client.
#[unsafe(no_mangle)]
#[cfg(not(feature = "broken"))]
pub extern "C" fn brood_actor_poison_epoch_run(
    _epoch: i64,
    inbox_ptr: i32,
    _inbox_cap: i32,
) -> i64 {
    let message_len = unsafe { read_message() };
    let Some(message) = guest_message(inbox_ptr, message_len) else {
        return 0;
    };
    if serde_json::from_slice::<serde_json::Value>(message).is_err() {
        return -2;
    }
    actor_outbox(serde_json::json!({
        "type": "spawn-app",
        "argv": ["weston-simple-egl", "-f"]
    }))
}

/// Deliberately invalid candidate used to prove that loading the second actor
/// cannot partially replace a live multi-actor generation.
#[unsafe(no_mangle)]
#[cfg(feature = "broken")]
pub extern "C" fn brood_actor_poison_epoch_run() -> i32 {
    0
}

fn actor_outbox(command: serde_json::Value) -> i64 {
    let Ok(bytes) = serde_json::to_vec(&serde_json::json!({
        "messages": [{"actor": "host", "payload": serde_json::to_vec(&command).unwrap_or_default()}],
        "publications": []
    })) else {
        return -2;
    };
    if bytes.is_empty() || bytes.len() > OUTBOX_CAPACITY {
        return -2;
    }
    let output = (&raw mut OUTBOX).cast::<u8>();
    unsafe { output.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
    let Ok(ptr) = i32::try_from(output.addr()) else {
        return -2;
    };
    let Ok(len) = i32::try_from(bytes.len()) else {
        return -2;
    };
    (i64::from(ptr) << 32) | i64::from(len)
}

fn guest_message(ptr: i32, len: i32) -> Option<&'static [u8]> {
    if ptr <= 0 || len <= 0 {
        return None;
    }
    Some(unsafe { slice::from_raw_parts(ptr as usize as *const u8, len as usize) })
}

fn arena_layout(capacity: i32, align: i32) -> Result<Layout, ()> {
    let capacity = usize::try_from(capacity).map_err(|_| ())?;
    let align = usize::try_from(align).map_err(|_| ())?;
    Layout::from_size_align(capacity, align).map_err(|_| ())
}
