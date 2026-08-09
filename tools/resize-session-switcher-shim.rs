#![no_std]

use core::{panic::PanicInfo, ptr};

const HEAP_BYTES: usize = 64 * 1024;
static mut HEAP: [u8; HEAP_BYTES] = [0; HEAP_BYTES];
static mut HEAP_CURSOR: usize = 0;

#[unsafe(no_mangle)]
pub extern "C" fn brood_arena_alloc(capacity: i32, align: i32) -> i32 {
    if capacity <= 0 || align <= 0 {
        return 0;
    }
    let capacity = capacity as usize;
    let align = align as usize;
    if !align.is_power_of_two() {
        return 0;
    }
    unsafe {
        let base = ptr::addr_of_mut!(HEAP).cast::<u8>() as usize;
        let cursor = (base + HEAP_CURSOR + align - 1) & !(align - 1);
        let Some(end) = cursor.checked_add(capacity) else {
            return 0;
        };
        if end - base > HEAP_BYTES {
            return 0;
        }
        HEAP_CURSOR = end - base;
        cursor as i32
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn brood_arena_free(_ptr: i32, _capacity: i32, _align: i32) {
    unsafe {
        HEAP_CURSOR = 0;
    }
}

// The production key policy can address the shell actor while the resize
// proof deliberately runs without the production client init tree.  A legacy
// actor has a mailbox but no presentation pane, so this sink keeps the proof
// inventory to exactly the two deterministic resize panes.
#[link(wasm_import_module = "brood")]
unsafe extern "C" {
    fn read_message() -> i32;
}

#[unsafe(export_name = "brood_actor_session-switcher_epoch_run")]
pub extern "C" fn discard_shell_message(_epoch: i64, _inbox_ptr: i32, _inbox_cap: i32) -> i64 {
    unsafe {
        read_message();
    }
    0
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
