#![no_std]

use core::panic::PanicInfo;

const KEY_STATE_PRESSED: i32 = 1;
const INPUT_MODE_NORMAL: i32 = 1;

const KEY_O: i32 = 32;
const KEY_Q: i32 = 24;

const KITTY: &[u8] = b"kitty";

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn close_active_window();
    fn spawn_app(ptr: *const u8, len: usize) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn on_key(
    key_code: i32,
    key_state: i32,
    input_mode: i32,
    ctrl_pressed: i32,
) -> i32 {
    if key_state != KEY_STATE_PRESSED || input_mode != INPUT_MODE_NORMAL || ctrl_pressed == 0 {
        return 0;
    }

    match key_code {
        KEY_O => unsafe { spawn_app(KITTY.as_ptr(), KITTY.len()) },
        KEY_Q => {
            unsafe { close_active_window() };
            1
        }
        _ => 0,
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
