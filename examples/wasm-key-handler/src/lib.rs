#![no_std]

use core::panic::PanicInfo;

const KEY_STATE_PRESSED: i32 = 1;
const INPUT_MODE_NORMAL: i32 = 1;

const KEY_O: i32 = 32;
const KEY_Q: i32 = 24;
const KEY_D: i32 = 40;
const KEY_F: i32 = 41;
const KEY_H: i32 = 43;
const KEY_J: i32 = 44;
const KEY_K: i32 = 45;
const KEY_L: i32 = 46;
const KEY_N: i32 = 57;
const KEY_P: i32 = 33;
const KEY_SPACE: i32 = 65;

const MOD_CTRL: i32 = 1;
const MOD_ALT: i32 = 2;

const KITTY: &[u8] = b"kitty";

const ACTION_FOCUS_NEXT: i32 = 1;
const ACTION_FOCUS_PREVIOUS: i32 = 2;
const ACTION_CLOSE_ACTIVE_PANE: i32 = 3;
const ACTION_TOGGLE_MANAGED_FULLSCREEN: i32 = 4;
const ACTION_SPLIT_ACTIVE_RIGHT: i32 = 5;
const ACTION_SPLIT_ACTIVE_DOWN: i32 = 6;
const ACTION_TOGGLE_LEFT_DOCK: i32 = 12;
const ACTION_TOGGLE_RIGHT_DOCK: i32 = 13;
const ACTION_TOGGLE_BOTTOM_DOCK: i32 = 14;

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn layout_action(action: i32) -> i32;
    fn spawn_app(ptr: *const u8, len: usize) -> i32;
    fn switch_active_session_next() -> i32;
    fn create_and_attach_session() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn on_key(key_code: i32, key_state: i32, input_mode: i32, ctrl_pressed: i32) -> i32 {
    let modifiers = if ctrl_pressed != 0 { MOD_CTRL } else { 0 };
    on_key_v2(key_code, key_state, input_mode, ctrl_pressed, modifiers)
}

#[unsafe(no_mangle)]
pub extern "C" fn on_key_v2(
    key_code: i32,
    key_state: i32,
    input_mode: i32,
    ctrl_pressed: i32,
    modifiers: i32,
) -> i32 {
    if key_state != KEY_STATE_PRESSED || input_mode != INPUT_MODE_NORMAL || ctrl_pressed == 0 {
        if key_state == KEY_STATE_PRESSED
            && input_mode == INPUT_MODE_NORMAL
            && modifiers & MOD_ALT != 0
            && key_code == KEY_SPACE
        {
            return unsafe { switch_active_session_next() };
        }
        return 0;
    }

    match key_code {
        KEY_N => unsafe { create_and_attach_session() },
        KEY_O => unsafe { spawn_app(KITTY.as_ptr(), KITTY.len()) },
        KEY_J => unsafe { layout_action(ACTION_FOCUS_NEXT) },
        KEY_K => unsafe { layout_action(ACTION_FOCUS_PREVIOUS) },
        KEY_Q => unsafe { layout_action(ACTION_CLOSE_ACTIVE_PANE) },
        KEY_F => unsafe { layout_action(ACTION_TOGGLE_MANAGED_FULLSCREEN) },
        KEY_L => unsafe { layout_action(ACTION_SPLIT_ACTIVE_RIGHT) },
        KEY_H => unsafe { layout_action(ACTION_SPLIT_ACTIVE_DOWN) },
        KEY_D => unsafe { layout_action(ACTION_TOGGLE_BOTTOM_DOCK) },
        KEY_P => unsafe {
            layout_action(ACTION_TOGGLE_LEFT_DOCK);
            layout_action(ACTION_TOGGLE_RIGHT_DOCK)
        },
        _ => 0,
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
