#![no_std]

use core::panic::PanicInfo;

const KEY_STATE_PRESSED: i32 = 1;
const INPUT_MODE_NORMAL: i32 = 1;

const KEY_1: i32 = 10;
const KEY_2: i32 = 11;
const KEY_3: i32 = 12;
const KEY_4: i32 = 13;
const KEY_5: i32 = 14;
const KEY_ENTER: i32 = 36;
const KEY_O: i32 = 32;
const KEY_Q: i32 = 24;
const KEY_D: i32 = 40;
const KEY_F: i32 = 41;
const KEY_G: i32 = 42;
const KEY_H: i32 = 43;
const KEY_J: i32 = 44;
const KEY_K: i32 = 45;
const KEY_L: i32 = 46;
const KEY_S: i32 = 39;
const KEY_U: i32 = 30;
const KEY_N: i32 = 57;
const KEY_P: i32 = 33;
const KEY_COMMA: i32 = 59;
const KEY_DOT: i32 = 60;
const KEY_SPACE: i32 = 65;

const MOD_CTRL: i32 = 1;
const MOD_ALT: i32 = 2;

const FOOT: &[u8] = b"foot";
const AGENT_APP_ID: &[u8] = b"manifold-codex";
const AGENT_COMMAND: &[u8] = b"foot --app-id manifold-codex codex --yolo";
const LAZYGIT_APP_ID: &[u8] = b"manifold-lazygit";
const LAZYGIT_COMMAND: &[u8] = b"foot --app-id manifold-lazygit lazygit";
const TERMINAL_1_APP_ID: &[u8] = b"manifold-terminal-1";
const TERMINAL_1_COMMAND: &[u8] = b"foot --app-id manifold-terminal-1";
const TERMINAL_2_APP_ID: &[u8] = b"manifold-terminal-2";
const TERMINAL_2_COMMAND: &[u8] = b"foot --app-id manifold-terminal-2";
const TERMINAL_3_APP_ID: &[u8] = b"manifold-terminal-3";
const TERMINAL_3_COMMAND: &[u8] = b"foot --app-id manifold-terminal-3";
const TERMINAL_4_APP_ID: &[u8] = b"manifold-terminal-4";
const TERMINAL_4_COMMAND: &[u8] = b"foot --app-id manifold-terminal-4";
const TERMINAL_5_APP_ID: &[u8] = b"manifold-terminal-5";
const TERMINAL_5_COMMAND: &[u8] = b"foot --app-id manifold-terminal-5";

static mut FOCUSED_PANE: i64 = -1;

const ACTION_FOCUS_NEXT: i32 = 1;
const ACTION_FOCUS_PREVIOUS: i32 = 2;
const ACTION_CLOSE_ACTIVE_PANE: i32 = 3;
const ACTION_TOGGLE_MANAGED_FULLSCREEN: i32 = 4;
const ACTION_SPLIT_ACTIVE_RIGHT: i32 = 5;
const ACTION_SPLIT_ACTIVE_DOWN: i32 = 6;
const ACTION_JOIN_ACTIVE_PANE: i32 = 7;
const ACTION_TOGGLE_LEFT_DOCK: i32 = 12;
const ACTION_TOGGLE_RIGHT_DOCK: i32 = 13;
const ACTION_TOGGLE_BOTTOM_DOCK: i32 = 14;
const ACTION_SPLIT_ACTIVE_SMART: i32 = 17;
const ACTION_FOCUS_NEXT_SPLIT: i32 = 18;
const ACTION_FOCUS_PREVIOUS_SPLIT: i32 = 19;

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn layout_action(action: i32) -> i32;
    fn focused_pane_id() -> i64;
    fn pane_for_app_id(app_id_ptr: *const u8, app_id_len: usize) -> i64;
    fn focus_pane(pane: i64) -> i32;
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
    sync_focused_pane_model();

    if modifiers & MOD_ALT != 0 && key_code == KEY_SPACE && input_mode == INPUT_MODE_NORMAL {
        return if key_state == KEY_STATE_PRESSED {
            unsafe { switch_active_session_next() }
        } else {
            1
        };
    }

    if modifiers & MOD_CTRL != 0 && is_global_ctrl_workspace_key(key_code) {
        return if key_state == KEY_STATE_PRESSED {
            handle_global_ctrl_workspace_key(key_code)
        } else {
            1
        };
    }

    if key_state != KEY_STATE_PRESSED || input_mode != INPUT_MODE_NORMAL || ctrl_pressed == 0 {
        return 0;
    }

    match key_code {
        KEY_N => unsafe { create_and_attach_session() },
        KEY_O => unsafe { spawn_app(FOOT.as_ptr(), FOOT.len()) },
        KEY_J => unsafe { layout_action(ACTION_FOCUS_NEXT) },
        KEY_K => unsafe { layout_action(ACTION_FOCUS_PREVIOUS) },
        KEY_Q => unsafe { layout_action(ACTION_CLOSE_ACTIVE_PANE) },
        KEY_F => unsafe { layout_action(ACTION_TOGGLE_MANAGED_FULLSCREEN) },
        KEY_L => unsafe { layout_action(ACTION_SPLIT_ACTIVE_RIGHT) },
        KEY_H => unsafe { layout_action(ACTION_SPLIT_ACTIVE_DOWN) },
        KEY_S => unsafe { layout_action(ACTION_SPLIT_ACTIVE_SMART) },
        KEY_U => unsafe { layout_action(ACTION_JOIN_ACTIVE_PANE) },
        KEY_DOT => unsafe { layout_action(ACTION_FOCUS_NEXT_SPLIT) },
        KEY_COMMA => unsafe { layout_action(ACTION_FOCUS_PREVIOUS_SPLIT) },
        KEY_D => unsafe { layout_action(ACTION_TOGGLE_BOTTOM_DOCK) },
        KEY_P => unsafe {
            layout_action(ACTION_TOGGLE_LEFT_DOCK);
            layout_action(ACTION_TOGGLE_RIGHT_DOCK)
        },
        _ => 0,
    }
}

fn is_global_ctrl_workspace_key(key_code: i32) -> bool {
    matches!(
        key_code,
        KEY_J
            | KEY_K
            | KEY_G
            | KEY_H
            | KEY_L
            | KEY_S
            | KEY_U
            | KEY_COMMA
            | KEY_DOT
            | KEY_ENTER
            | KEY_1
            | KEY_2
            | KEY_3
            | KEY_4
            | KEY_5
    )
}

fn handle_global_ctrl_workspace_key(key_code: i32) -> i32 {
    match key_code {
        KEY_J => unsafe { layout_action(ACTION_FOCUS_NEXT) },
        KEY_K => unsafe { layout_action(ACTION_FOCUS_PREVIOUS) },
        KEY_H => unsafe { layout_action(ACTION_SPLIT_ACTIVE_DOWN) },
        KEY_L => unsafe { layout_action(ACTION_SPLIT_ACTIVE_RIGHT) },
        KEY_S => unsafe { layout_action(ACTION_SPLIT_ACTIVE_SMART) },
        KEY_U => unsafe { layout_action(ACTION_JOIN_ACTIVE_PANE) },
        KEY_DOT => unsafe { layout_action(ACTION_FOCUS_NEXT_SPLIT) },
        KEY_COMMA => unsafe { layout_action(ACTION_FOCUS_PREVIOUS_SPLIT) },
        KEY_G => focus_or_spawn(LAZYGIT_APP_ID, LAZYGIT_COMMAND),
        KEY_ENTER => focus_or_spawn(AGENT_APP_ID, AGENT_COMMAND),
        KEY_1 => focus_or_spawn(TERMINAL_1_APP_ID, TERMINAL_1_COMMAND),
        KEY_2 => focus_or_spawn(TERMINAL_2_APP_ID, TERMINAL_2_COMMAND),
        KEY_3 => focus_or_spawn(TERMINAL_3_APP_ID, TERMINAL_3_COMMAND),
        KEY_4 => focus_or_spawn(TERMINAL_4_APP_ID, TERMINAL_4_COMMAND),
        KEY_5 => focus_or_spawn(TERMINAL_5_APP_ID, TERMINAL_5_COMMAND),
        _ => 0,
    }
}

fn focus_or_spawn(app_id: &[u8], command: &[u8]) -> i32 {
    let pane = unsafe { pane_for_app_id(app_id.as_ptr(), app_id.len()) };
    if pane > 0 {
        let changed = unsafe { focus_pane(pane) };
        if changed != 0 {
            unsafe {
                FOCUSED_PANE = pane;
            }
        }
        changed
    } else {
        unsafe { spawn_app(command.as_ptr(), command.len()) }
    }
}

fn sync_focused_pane_model() {
    let pane = unsafe { focused_pane_id() };
    unsafe {
        FOCUSED_PANE = pane;
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
