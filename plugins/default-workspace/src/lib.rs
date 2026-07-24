#![cfg_attr(not(test), no_std)]

#[cfg(not(test))]
use core::panic::PanicInfo;
use core::{ptr, slice};
use serde::Deserialize;
use serde_json_core::heapless::{String as FixedString, Vec as FixedVec};

const HEAP_BYTES: usize = 256 * 1024;
const RESPONSE_BYTES: usize = 64 * 1024;
const OUTBOX_BYTES: usize = 64 * 1024;
const MAX_PANES: usize = 64;
const MAX_APP_ID_BYTES: usize = 256;
const MAX_MESSAGE_KIND_BYTES: usize = 64;
const JSON_UNESCAPE_BYTES: usize = 512;
const OUTPUT_ERROR: i32 = -1;
const OUTPUT_ABSTAIN: i32 = 0;
const OUTPUT_OK: i32 = 1;

const KEY_RELEASED: i32 = 0;
const KEY_PRESSED: i32 = 1;
const INPUT_MODE_PASSTHROUGH: i32 = 0;
const INPUT_MODE_NORMAL: i32 = 1;

const MOD_CTRL: i32 = 1;
const MOD_ALT: i32 = 2;
const MOD_SHIFT: i32 = 4;

const KEY_ESC: i32 = 1;
const KEY_1: i32 = 10;
const KEY_2: i32 = 11;
const KEY_3: i32 = 12;
const KEY_4: i32 = 13;
const KEY_5: i32 = 14;
const KEY_ENTER: i32 = 36;
const KEY_O: i32 = 32;
const KEY_B: i32 = 56;
const KEY_M: i32 = 58;
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
const KEY_W: i32 = 25;
const KEY_N: i32 = 57;
const KEY_P: i32 = 33;
const KEY_COMMA: i32 = 59;
const KEY_DOT: i32 = 60;
const KEY_SPACE: i32 = 65;

static mut HEAP: [u8; HEAP_BYTES] = [0; HEAP_BYTES];
static mut HEAP_CURSOR: usize = 0;
static mut RESPONSE: [u8; RESPONSE_BYTES] = [0; RESPONSE_BYTES];
static mut RESPONSE_DESC: [i32; 4] = [0; 4];
static mut OUTBOX: [u8; OUTBOX_BYTES] = [0; OUTBOX_BYTES];
static mut WORKSPACE_INPUT_MODE: i32 = INPUT_MODE_PASSTHROUGH;
static mut LAYOUT_PANES: [u64; 64] = [0; 64];
static mut LAYOUT_PANE_COUNT: usize = 0;
static mut LAYOUT_FOCUS: u64 = 0;
static mut LAYOUT_HAS_FOCUS: bool = false;
static mut LAYOUT_FULLSCREEN: bool = false;
static mut LAYOUT_LEASE_ID: u64 = 0;
static mut LAYOUT_LEASE_GENERATION: u64 = 0;
static mut LAYOUT_REVISION: u64 = 0;
static mut LAYOUT_HAS_LEASE: bool = false;

type PaneList = FixedVec<u64, MAX_PANES>;
type AppPaneList = FixedVec<AppPane, MAX_PANES>;

#[derive(Default, Deserialize)]
struct PolicyState {
    #[serde(default)]
    focused_pane: Option<u64>,
    #[serde(default)]
    panes: PaneList,
    #[serde(default)]
    managed_panes: PaneList,
    #[serde(default)]
    app_panes: AppPaneList,
}

#[derive(Deserialize)]
struct AppPane {
    pane: u64,
    app_id: FixedString<MAX_APP_ID_BYTES>,
}

#[derive(Default, Deserialize)]
struct WorkspaceMessage {
    #[serde(rename = "type")]
    message_type: FixedString<MAX_MESSAGE_KIND_BYTES>,
    #[serde(default)]
    action: Option<FixedString<MAX_MESSAGE_KIND_BYTES>>,
    #[serde(default)]
    visible_panes: PaneList,
    #[serde(default)]
    focused_pane: Option<u64>,
    #[serde(default)]
    pane_app_ids: AppPaneList,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    lease: Option<LayoutLeaseToken>,
}

#[derive(Clone, Copy, Deserialize)]
struct LayoutLeaseToken {
    id: u64,
    generation: u64,
}

fn decode_json<T>(bytes: &[u8]) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut unescape = [0_u8; JSON_UNESCAPE_BYTES];
    let (value, consumed) = serde_json_core::from_slice_escaped::<T>(bytes, &mut unescape).ok()?;
    (consumed == bytes.len()).then_some(value)
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn read_message() -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn read_message() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn manifold_arena_alloc(capacity: i32, align: i32) -> i32 {
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
        let cursor = align_up(base + HEAP_CURSOR, align) - base;
        let Some(end) = cursor.checked_add(capacity) else {
            return 0;
        };
        if end > HEAP_BYTES {
            return 0;
        }
        HEAP_CURSOR = end;
        (base + cursor) as i32
    }
}

#[no_mangle]
pub extern "C" fn manifold_arena_free(_ptr: i32, _capacity: i32, _align: i32) {}

#[no_mangle]
pub extern "C" fn manifold_policy_provider_key_workspace(
    query_ptr: i32,
    query_len: i32,
    state_ptr: i32,
    state_len: i32,
) -> i32 {
    let Some(query) = guest_slice(query_ptr, query_len) else {
        return OUTPUT_ERROR;
    };
    let Some(input) = decode_json::<KeyInput>(query) else {
        return OUTPUT_ERROR;
    };
    let state_bytes = guest_slice(state_ptr, state_len).unwrap_or(&[]);
    let state = if state_bytes.is_empty() {
        PolicyState::default()
    } else {
        let Some(state) = decode_json::<PolicyState>(state_bytes) else {
            return OUTPUT_ERROR;
        };
        state
    };
    let output = response_buffer_mut();

    if input.modifiers & MOD_CTRL != 0
        && input.modifiers & MOD_SHIFT != 0
        && input.key_code == KEY_ESC
    {
        return if input.key_state == KEY_RELEASED {
            write_consume(output, &[])
        } else {
            write_consume(output, &[Message::ToggleShortcutPassthrough])
        };
    }

    if input.modifiers & MOD_CTRL != 0 && input.key_code == KEY_B {
        if input.key_state == KEY_PRESSED {
            toggle_workspace_input_mode();
        }
        return write_consume(output, &[]);
    }

    if input.modifiers & MOD_CTRL != 0 && input.modifiers & MOD_ALT != 0 && input.key_code == KEY_M
    {
        return if input.key_state == KEY_RELEASED || input.repeat {
            write_consume(output, &[])
        } else {
            write_consume(output, &[Message::SessionSwitcherAction("toggle")])
        };
    }

    let workspace_input_mode = workspace_input_mode();

    if input.modifiers & MOD_ALT != 0
        && input.key_code == KEY_SPACE
        && workspace_input_mode == INPUT_MODE_NORMAL
    {
        return if input.key_state == KEY_RELEASED {
            write_consume(output, &[])
        } else {
            write_consume(output, &[Message::SwitchActiveSessionNext])
        };
    }

    if input.modifiers & MOD_CTRL != 0 && is_global_workspace_key(input.key_code) {
        return if input.key_state == KEY_RELEASED {
            write_consume(output, &[])
        } else {
            let mut buffer = [Message::Noop; 1];
            write_consume(
                output,
                messages_for_global_key(input.key_code, &state, &mut buffer),
            )
        };
    }

    if input.key_state != KEY_PRESSED
        || workspace_input_mode != INPUT_MODE_NORMAL
        || input.modifiers & MOD_CTRL == 0
    {
        return OUTPUT_ABSTAIN;
    }

    let messages = match input.key_code {
        KEY_N => &[Message::CreateAndAttachSession][..],
        KEY_O => &[Message::SpawnApp(&["foot"])][..],
        KEY_W => &[Message::SpawnApp(&[
            "flatpak",
            "run",
            "--env=GDK_BACKEND=wayland",
            "--env=QT_QPA_PLATFORM=wayland",
            "org.wezfurlong.wezterm",
        ])][..],
        KEY_J => &[Message::LayoutAction("focus-next")][..],
        KEY_K => &[Message::LayoutAction("focus-previous")][..],
        KEY_Q => &[Message::LayoutAction("close-active-pane")][..],
        KEY_F => &[Message::LayoutAction("toggle-managed-fullscreen")][..],
        KEY_L => &[Message::LayoutAction("split-active-right")][..],
        KEY_H => &[Message::LayoutAction("split-active-down")][..],
        KEY_S => &[Message::LayoutAction("split-active-smart")][..],
        KEY_U => &[Message::LayoutAction("join-active-pane")][..],
        KEY_DOT => &[Message::LayoutAction("focus-next-split")][..],
        KEY_COMMA => &[Message::LayoutAction("focus-previous-split")][..],
        KEY_D => &[Message::LayoutAction("toggle-bottom-dock")][..],
        KEY_P => &[
            Message::LayoutAction("toggle-left-dock"),
            Message::LayoutAction("toggle-right-dock"),
        ][..],
        _ => return write_consume(output, &[]),
    };
    write_consume(output, messages)
}

#[no_mangle]
pub extern "C" fn manifold_actor_workspace_epoch_run(
    _epoch: i64,
    inbox_ptr: i32,
    _inbox_cap: i32,
) -> i64 {
    let message_len = unsafe { read_message() };
    if message_len <= 0 {
        return 0;
    }
    let Some(message) = guest_slice(inbox_ptr, message_len) else {
        return -2;
    };
    let Some(message) = decode_json::<WorkspaceMessage>(message) else {
        return -2;
    };
    let output = outbox_buffer_mut();
    if message.message_type == "layout-state" {
        store_layout_actor_state(
            &message.visible_panes,
            message.focused_pane,
            message.lease,
            message.revision,
        );
        let len = write_layout_actor_outbox(
            output,
            &message.pane_app_ids,
            &message.visible_panes,
            message.focused_pane,
            layout_actor_fullscreen(),
            message.lease,
            message.revision,
        );
        if len <= 0 {
            return -2;
        }
        return pack_actor_outbox(output.as_ptr() as i32, len);
    }

    if message.message_type != "layout-action" {
        return 0;
    }
    apply_layout_actor_action(message.action.as_deref().unwrap_or(""));
    let (panes, focus, fullscreen, lease, revision) = layout_actor_state();
    let len = write_layout_actor_update_outbox(output, panes, focus, fullscreen, lease, revision);
    if len <= 0 {
        return -2;
    }
    pack_actor_outbox(output.as_ptr() as i32, len)
}

#[no_mangle]
pub extern "C" fn manifold_actor_workspace_subscribe_layout_state() {}

fn messages_for_global_key<'a>(
    key_code: i32,
    state: &PolicyState,
    buffer: &'a mut [Message<'static>; 1],
) -> &'a [Message<'static>] {
    match key_code {
        KEY_J => focus_cycle_managed_pane(state, true, buffer),
        KEY_K => focus_cycle_managed_pane(state, false, buffer),
        KEY_O => &[Message::SpawnApp(&["foot"])],
        KEY_H => &[Message::LayoutAction("split-active-down")],
        KEY_L => &[Message::LayoutAction("split-active-right")],
        KEY_S => &[Message::LayoutAction("split-active-smart")],
        KEY_U => &[Message::LayoutAction("join-active-pane")],
        KEY_DOT => focus_cycle_terminal(state, true, buffer)
            .unwrap_or(&[Message::LayoutAction("focus-next-split")]),
        KEY_COMMA => focus_cycle_terminal(state, false, buffer)
            .unwrap_or(&[Message::LayoutAction("focus-previous-split")]),
        KEY_G => &[Message::FocusOrSpawnApp {
            app_id: "manifold-lazygit",
            argv: &["foot", "--app-id", "manifold-lazygit", "lazygit"],
        }],
        KEY_ENTER => focus_or_spawn_app(
            state,
            b"pi-foot",
            &["foot", "--app-id", "pi-foot", "pi"],
            buffer,
        ),
        KEY_1 => focus_or_spawn_app(
            state,
            b"manifold-terminal-1",
            &["foot", "--app-id", "manifold-terminal-1"],
            buffer,
        ),
        KEY_2 => focus_or_spawn_app(
            state,
            b"manifold-terminal-2",
            &["foot", "--app-id", "manifold-terminal-2"],
            buffer,
        ),
        KEY_3 => focus_or_spawn_app(
            state,
            b"manifold-terminal-3",
            &["foot", "--app-id", "manifold-terminal-3"],
            buffer,
        ),
        KEY_4 => focus_or_spawn_app(
            state,
            b"manifold-terminal-4",
            &["foot", "--app-id", "manifold-terminal-4"],
            buffer,
        ),
        KEY_5 => focus_or_spawn_app(
            state,
            b"manifold-terminal-5",
            &["foot", "--app-id", "manifold-terminal-5"],
            buffer,
        ),
        _ => &[],
    }
}

fn focus_or_spawn_app<'a>(
    state: &PolicyState,
    app_id: &'static [u8],
    argv: &'static [&'static str],
    buffer: &'a mut [Message<'static>; 1],
) -> &'a [Message<'static>] {
    if let Some(pane) = pane_for_app_id(state, app_id) {
        buffer[0] = Message::FocusPane(pane);
    } else {
        buffer[0] = Message::SpawnApp(argv);
    }
    buffer
}

fn focus_cycle_terminal<'a>(
    state: &PolicyState,
    next: bool,
    buffer: &'a mut [Message<'static>; 1],
) -> Option<&'a [Message<'static>]> {
    let mut panes = [0_u64; 5];
    let mut count = 0;
    for app_id in [
        b"manifold-terminal-1" as &[u8],
        b"manifold-terminal-2" as &[u8],
        b"manifold-terminal-3" as &[u8],
        b"manifold-terminal-4" as &[u8],
        b"manifold-terminal-5" as &[u8],
    ] {
        if let Some(pane) = pane_for_app_id(state, app_id) {
            panes[count] = pane;
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }

    let focused = state.focused_pane;
    let current = focused.and_then(|focused| {
        let mut index = 0;
        while index < count {
            if panes[index] == focused {
                return Some(index);
            }
            index += 1;
        }
        None
    });
    let target = match (current, next) {
        (Some(index), true) => (index + 1) % count,
        (Some(0), false) => count - 1,
        (Some(index), false) => index - 1,
        (None, true) => 0,
        (None, false) => count - 1,
    };
    buffer[0] = Message::FocusPane(panes[target]);
    Some(buffer)
}

fn focus_cycle_managed_pane<'a>(
    state: &PolicyState,
    next: bool,
    buffer: &'a mut [Message<'static>; 1],
) -> &'a [Message<'static>] {
    let panes = if state.managed_panes.is_empty() {
        &state.panes
    } else {
        &state.managed_panes
    };
    if panes.is_empty() {
        return &[];
    }
    let current = state
        .focused_pane
        .and_then(|focused| panes.iter().position(|pane| *pane == focused));
    let target = match (current, next) {
        (Some(index), true) => (index + 1) % panes.len(),
        (Some(0), false) => panes.len() - 1,
        (Some(index), false) => index - 1,
        (None, true) => 0,
        (None, false) => panes.len() - 1,
    };
    buffer[0] = Message::FocusPane(panes[target]);
    buffer
}

fn is_global_workspace_key(key_code: i32) -> bool {
    matches!(
        key_code,
        KEY_J
            | KEY_K
            | KEY_O
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

fn workspace_input_mode() -> i32 {
    unsafe { WORKSPACE_INPUT_MODE }
}

fn toggle_workspace_input_mode() {
    unsafe {
        WORKSPACE_INPUT_MODE = if WORKSPACE_INPUT_MODE == INPUT_MODE_NORMAL {
            INPUT_MODE_PASSTHROUGH
        } else {
            INPUT_MODE_NORMAL
        };
    }
}

#[derive(Clone, Copy)]
enum Message<'a> {
    Noop,
    LayoutAction(&'a str),
    SpawnApp(&'a [&'a str]),
    FocusPane(u64),
    FocusOrSpawnApp {
        app_id: &'a str,
        argv: &'a [&'a str],
    },
    SwitchActiveSessionNext,
    CreateAndAttachSession,
    ToggleShortcutPassthrough,
    SessionSwitcherAction(&'a str),
}

fn write_layout_actor_outbox(
    output: &mut [u8],
    app_panes: &[AppPane],
    panes: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":[{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    write_set_resolved_layout_payload(&mut payload, panes, focus, fullscreen, lease, revision);
    writer.bytes(br#"]}],"publications":[{"payload":["#);
    let mut publication = PayloadWriter::new(&mut writer);
    publication.bytes(br#"{"kind":"workspace-state","mode":"#);
    publication.u64(workspace_input_mode() as u64);
    publication.bytes(br#", "focused_pane":"#);
    if let Some(focus) = focus {
        publication.u64(focus);
    } else {
        publication.bytes(b"null");
    }
    publication.bytes(br#", "focus_rules":[{"key":36,"modifiers":1,"app_id":"pi-foot","argv":["foot","--app-id","pi-foot","pi"]},{"key":10,"modifiers":1,"app_id":"manifold-terminal-1","argv":["foot","--app-id","manifold-terminal-1"]},{"key":11,"modifiers":1,"app_id":"manifold-terminal-2","argv":["foot","--app-id","manifold-terminal-2"]},{"key":12,"modifiers":1,"app_id":"manifold-terminal-3","argv":["foot","--app-id","manifold-terminal-3"]},{"key":13,"modifiers":1,"app_id":"manifold-terminal-4","argv":["foot","--app-id","manifold-terminal-4"]},{"key":14,"modifiers":1,"app_id":"manifold-terminal-5","argv":["foot","--app-id","manifold-terminal-5"]}], "panes":["#);
    let mut index = 0;
    while index < panes.len() {
        if index > 0 {
            publication.byte(b',');
        }
        publication.u64(panes[index]);
        index += 1;
    }
    publication.bytes(br#"],"app_panes":["#);
    let mut first_app_pane = true;
    write_app_pane(&mut publication, app_panes, b"pi-foot", &mut first_app_pane);
    write_app_pane(
        &mut publication,
        app_panes,
        b"manifold-terminal-1",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"manifold-terminal-2",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"manifold-terminal-3",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"manifold-terminal-4",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"manifold-terminal-5",
        &mut first_app_pane,
    );
    publication.bytes(br#"]}"#);
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

fn write_layout_actor_update_outbox(
    output: &mut [u8],
    panes: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":[{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    write_set_resolved_layout_payload(&mut payload, panes, focus, fullscreen, lease, revision);
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

fn store_layout_actor_state(
    panes: &[u64],
    focus: Option<u64>,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) {
    let pane_count = panes.len().min(64);
    unsafe {
        let target = ptr::addr_of_mut!(LAYOUT_PANES).cast::<u64>();
        let mut index = 0;
        while index < pane_count {
            target.add(index).write(panes[index]);
            index += 1;
        }
        LAYOUT_PANE_COUNT = pane_count;
        let accepted_focus = focus.filter(|focus| panes[..pane_count].contains(focus));
        LAYOUT_FOCUS = accepted_focus
            .or_else(|| panes.first().copied())
            .unwrap_or(0);
        LAYOUT_HAS_FOCUS = pane_count > 0;
        if pane_count == 0 {
            LAYOUT_FULLSCREEN = false;
        }
        if let (Some(lease), Some(revision)) = (lease, revision) {
            LAYOUT_LEASE_ID = lease.id;
            LAYOUT_LEASE_GENERATION = lease.generation;
            LAYOUT_REVISION = revision;
            LAYOUT_HAS_LEASE = true;
        } else {
            LAYOUT_HAS_LEASE = false;
        }
    }
}

fn layout_actor_fullscreen() -> bool {
    unsafe { LAYOUT_FULLSCREEN }
}

fn layout_actor_state() -> (
    &'static [u64],
    Option<u64>,
    bool,
    Option<LayoutLeaseToken>,
    Option<u64>,
) {
    unsafe {
        let pane_count = LAYOUT_PANE_COUNT.min(64);
        let panes = slice::from_raw_parts(ptr::addr_of!(LAYOUT_PANES).cast::<u64>(), pane_count);
        let focus = LAYOUT_HAS_FOCUS.then_some(LAYOUT_FOCUS);
        let lease = LAYOUT_HAS_LEASE.then_some(LayoutLeaseToken {
            id: LAYOUT_LEASE_ID,
            generation: LAYOUT_LEASE_GENERATION,
        });
        let revision = LAYOUT_HAS_LEASE.then_some(LAYOUT_REVISION);
        (panes, focus, LAYOUT_FULLSCREEN, lease, revision)
    }
}

fn apply_layout_actor_action(action: &str) {
    let next = matches!(action, "focus-next" | "focus-next-split");
    let previous = matches!(action, "focus-previous" | "focus-previous-split");
    unsafe {
        let pane_count = LAYOUT_PANE_COUNT.min(64);
        if pane_count == 0 {
            return;
        }
        let panes = slice::from_raw_parts(ptr::addr_of!(LAYOUT_PANES).cast::<u64>(), pane_count);
        if action == "toggle-managed-fullscreen" {
            LAYOUT_FULLSCREEN = !LAYOUT_FULLSCREEN;
            return;
        }
        if !next && !previous {
            return;
        }
        let current = panes
            .iter()
            .position(|pane| LAYOUT_HAS_FOCUS && *pane == LAYOUT_FOCUS);
        let target = if next {
            current.map_or(0, |index| (index + 1) % pane_count)
        } else {
            current.map_or(pane_count - 1, |index| {
                if index == 0 {
                    pane_count - 1
                } else {
                    index - 1
                }
            })
        };
        LAYOUT_FOCUS = panes[target];
        LAYOUT_HAS_FOCUS = true;
    }
}

fn write_app_pane(
    publication: &mut PayloadWriter<'_, '_>,
    app_panes: &[AppPane],
    app_id: &[u8],
    first: &mut bool,
) {
    let Some(pane) = pane_for_app_id_in_list(app_panes, app_id) else {
        return;
    };
    if !*first {
        publication.byte(b',');
    }
    *first = false;
    publication.bytes(br#"{"pane":"#);
    publication.u64(pane);
    publication.bytes(br#", "app_id":""#);
    publication.quoted_body(app_id);
    publication.bytes(br#""}"#);
}

fn write_set_resolved_layout_payload(
    payload: &mut PayloadWriter<'_, '_>,
    panes: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) {
    payload.bytes(br#"{"type":"set-resolved-layout""#);
    if let (Some(lease), Some(revision)) = (lease, revision) {
        payload.bytes(br#","lease":{"id":"#);
        payload.u64(lease.id);
        payload.bytes(br#","generation":"#);
        payload.u64(lease.generation);
        payload.bytes(br#"},"revision":"#);
        payload.u64(revision);
    }
    payload.bytes(br#","layout":{"items":["#);
    let pane_count = panes.len();
    if fullscreen && pane_count > 0 {
        let pane = focus
            .filter(|focus| panes.contains(focus))
            .unwrap_or(panes[0]);
        write_pane_item_payload(payload, pane, 0.0, -1.0, -1.0, 2.0, 2.0);
    } else if pane_count == 1 {
        write_pane_item_payload(payload, panes[0], 0.0, -1.0, -1.0, 2.0, 2.0);
    } else if pane_count > 1 {
        write_pane_item_payload(payload, panes[0], 0.0, -1.0, -1.0, 1.0, 2.0);
        let each = 2.0 / ((pane_count - 1) as f32);
        let mut index = 1;
        while index < pane_count {
            payload.byte(b',');
            write_pane_item_payload(
                payload,
                panes[index],
                index as f32,
                0.0,
                -1.0 + each * ((index - 1) as f32),
                1.0,
                each,
            );
            index += 1;
        }
    }
    payload.bytes(br#"]},"focus":"#);
    if let Some(focus) = focus {
        payload.u64(focus);
    } else {
        payload.bytes(b"null");
    }
    payload.byte(b'}');
}

fn write_pane_item_payload(
    payload: &mut PayloadWriter<'_, '_>,
    pane: u64,
    z: f32,
    offset_u: f32,
    offset_v: f32,
    size_u: f32,
    size_v: f32,
) {
    let mut item = [0_u8; 512];
    let mut writer = Writer::new(&mut item);
    write_pane_item(&mut writer, pane, z, offset_u, offset_v, size_u, size_v);
    let len = writer.finish();
    if len > 0 {
        payload.bytes(&item[..len as usize]);
    }
}

fn write_consume(output: &mut [u8], messages: &[Message<'_>]) -> i32 {
    finish_response(write_consume_payload(output, messages))
}

fn write_consume_payload(output: &mut [u8], messages: &[Message<'_>]) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"decision":"consume","messages":["#);
    let mut index = 0;
    while index < messages.len() {
        if index > 0 {
            writer.byte(b',');
        }
        let actor = match messages[index] {
            Message::LayoutAction("close-active-pane") => "host",
            Message::LayoutAction(_) => "workspace",
            Message::SessionSwitcherAction(_) => "session-switcher",
            _ => "host",
        };
        writer.bytes(br#"{"actor":""#);
        writer.bytes(actor.as_bytes());
        writer.bytes(br#"","payload":["#);
        let mut payload = PayloadWriter::new(&mut writer);
        match messages[index] {
            Message::Noop => {}
            Message::LayoutAction(action) => {
                payload.bytes(br#"{"type":"layout-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
            Message::SpawnApp(argv) => {
                payload.bytes(br#"{"type":"spawn-app","argv":"#);
                payload.argv(argv);
                payload.byte(b'}');
            }
            Message::FocusPane(pane) => {
                payload.bytes(br#"{"type":"focus-pane","pane":"#);
                payload.u64(pane);
                payload.byte(b'}');
            }
            Message::FocusOrSpawnApp { app_id, argv } => {
                payload.bytes(br#"{"type":"focus-or-spawn-app","app-id":"#);
                payload.byte(b'"');
                payload.quoted_body(app_id.as_bytes());
                payload.bytes(br#"","argv":"#);
                payload.argv(argv);
                payload.byte(b'}');
            }
            Message::SwitchActiveSessionNext => {
                payload.bytes(br#"{"type":"switch-active-session-next"}"#)
            }
            Message::CreateAndAttachSession => {
                payload.bytes(br#"{"type":"create-and-attach-session"}"#)
            }
            Message::ToggleShortcutPassthrough => {
                payload.bytes(br#"{"type":"toggle-shortcut-passthrough"}"#)
            }
            Message::SessionSwitcherAction(action) => {
                payload.bytes(br#"{"type":"sidebar-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
        }
        writer.bytes(br#"]}"#);
        index += 1;
    }
    writer.bytes(br#"]}"#);
    writer.finish()
}

fn write_pane_item(
    writer: &mut Writer<'_>,
    pane: u64,
    z: f32,
    offset_u: f32,
    offset_v: f32,
    size_u: f32,
    size_v: f32,
) {
    writer.bytes(br#"{"id":"#);
    writer.u64(pane);
    writer.bytes(br#", "kind":{"type":"pane","pane":"#);
    writer.u64(pane);
    writer.bytes(br#"},"z":"#);
    writer.f32(z);
    writer.bytes(br#", "transform":{"offset":{"u":"#);
    writer.f32(offset_u);
    writer.bytes(br#", "v":"#);
    writer.f32(offset_v);
    writer.bytes(br#"},"size":{"u":"#);
    writer.f32(size_u);
    writer.bytes(br#", "v":"#);
    writer.f32(size_v);
    writer.bytes(br#"}},"input":"pane"}"#);
}

#[derive(Deserialize)]
struct KeyInput {
    key_code: i32,
    key_state: i32,
    modifiers: i32,
    #[serde(default)]
    repeat: bool,
}

struct Writer<'a> {
    output: &'a mut [u8],
    len: usize,
    overflowed: bool,
}

impl<'a> Writer<'a> {
    fn new(output: &'a mut [u8]) -> Self {
        Self {
            output,
            len: 0,
            overflowed: false,
        }
    }

    fn finish(self) -> i32 {
        if self.overflowed {
            OUTPUT_ERROR
        } else {
            self.len as i32
        }
    }

    fn byte(&mut self, byte: u8) {
        if self.len >= self.output.len() {
            self.overflowed = true;
            return;
        }
        self.output[self.len] = byte;
        self.len += 1;
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.byte(*byte);
        }
    }

    fn u64(&mut self, value: u64) {
        write_u64(self, value);
    }

    fn f32(&mut self, value: f32) {
        let mut value = value;
        if value < 0.0 {
            self.byte(b'-');
            value = -value;
        }
        let whole = value as u64;
        self.u64(whole);
        let frac = ((value - (whole as f32)) * 1000.0) as u64;
        if frac > 0 {
            self.byte(b'.');
            self.byte(b'0' + ((frac / 100) % 10) as u8);
            self.byte(b'0' + ((frac / 10) % 10) as u8);
            self.byte(b'0' + (frac % 10) as u8);
        }
    }
}

struct PayloadWriter<'a, 'b> {
    output: &'a mut Writer<'b>,
    first: bool,
}

impl<'a, 'b> PayloadWriter<'a, 'b> {
    fn new(output: &'a mut Writer<'b>) -> Self {
        Self {
            output,
            first: true,
        }
    }

    fn byte(&mut self, byte: u8) {
        if !self.first {
            self.output.byte(b',');
        }
        self.first = false;
        self.output.u64(byte as u64);
    }

    fn u64(&mut self, mut value: u64) {
        let mut digits = [0_u8; 20];
        let mut len = 0;
        loop {
            digits[len] = b'0' + (value % 10) as u8;
            len += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while len > 0 {
            len -= 1;
            self.byte(digits[len]);
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.byte(*byte);
        }
    }

    fn quoted_body(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if *byte == b'"' || *byte == b'\\' {
                self.byte(b'\\');
            }
            self.byte(*byte);
        }
    }

    fn argv(&mut self, argv: &[&str]) {
        self.byte(b'[');
        let mut index = 0;
        while index < argv.len() {
            if index > 0 {
                self.byte(b',');
            }
            self.byte(b'"');
            self.quoted_body(argv[index].as_bytes());
            self.byte(b'"');
            index += 1;
        }
        self.byte(b']');
    }
}

fn write_u64(writer: &mut Writer<'_>, mut value: u64) {
    let mut digits = [0_u8; 20];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        writer.byte(digits[len]);
    }
}

fn pane_for_app_id(state: &PolicyState, app_id: &[u8]) -> Option<u64> {
    pane_for_app_id_in_list(&state.app_panes, app_id)
}

fn pane_for_app_id_in_list(app_panes: &[AppPane], app_id: &[u8]) -> Option<u64> {
    app_panes
        .iter()
        .find_map(|item| (item.app_id.as_bytes() == app_id).then_some(item.pane))
}

fn guest_slice(ptr: i32, len: i32) -> Option<&'static [u8]> {
    if ptr < 0 || len < 0 {
        return None;
    }
    Some(unsafe { slice::from_raw_parts(ptr as usize as *const u8, len as usize) })
}

fn response_buffer_mut() -> &'static mut [u8] {
    unsafe { slice::from_raw_parts_mut(ptr::addr_of_mut!(RESPONSE).cast::<u8>(), RESPONSE_BYTES) }
}

fn outbox_buffer_mut() -> &'static mut [u8] {
    unsafe { slice::from_raw_parts_mut(ptr::addr_of_mut!(OUTBOX).cast::<u8>(), OUTBOX_BYTES) }
}

fn finish_response(written: i32) -> i32 {
    if written <= 0 {
        return written;
    }
    unsafe {
        RESPONSE_DESC[0] = OUTPUT_OK;
        RESPONSE_DESC[1] = ptr::addr_of_mut!(RESPONSE).cast::<u8>() as i32;
        RESPONSE_DESC[2] = written;
        RESPONSE_DESC[3] = 0;
        ptr::addr_of_mut!(RESPONSE_DESC).cast::<i32>() as i32
    }
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn pack_actor_outbox(ptr: i32, len: i32) -> i64 {
    if ptr <= 0 || len <= 0 {
        return -2;
    }
    ((ptr as i64) << 32) | (len as u32 as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_state_json_accepts_whitespace_ordering_and_escapes() {
        let state = decode_json::<PolicyState>(
            br#"{
                "app_panes": [{"app_id": "manifold-\u0074erminal-1", "pane": 9}],
                "ignored": {"nested": [true, null, "value"]},
                "managed_panes": [9, 10],
                "focused_pane": 10,
                "panes": [18446744073709551615, 9, 10]
            }"#,
        )
        .expect("decode policy state");

        assert_eq!(state.focused_pane, Some(10));
        assert_eq!(state.panes.as_slice(), &[u64::MAX, 9, 10]);
        assert_eq!(state.managed_panes.as_slice(), &[9, 10]);
        assert_eq!(pane_for_app_id(&state, b"manifold-terminal-1"), Some(9));
    }

    #[test]
    fn workspace_message_json_is_structural_not_substring_based() {
        let message = decode_json::<WorkspaceMessage>(
            br#"{
                "action": "focus-next",
                "metadata": "\"type\":\"layout-state\"",
                "type": "layout-action"
            }"#,
        )
        .expect("decode workspace message");

        assert_eq!(message.message_type.as_str(), "layout-action");
        assert_eq!(message.action.as_deref(), Some("focus-next"));
        assert!(message.visible_panes.is_empty());
    }

    #[test]
    fn malformed_or_trailing_json_is_rejected() {
        assert!(decode_json::<PolicyState>(br#"{"panes":[1,]}"#).is_none());
        assert!(decode_json::<PolicyState>(br#"{"panes":[1]} trailing"#).is_none());
    }

    #[test]
    fn session_switcher_action_is_an_ordinary_actor_message() {
        let mut output = [0_u8; 512];
        let len = write_consume_payload(&mut output, &[Message::SessionSwitcherAction("toggle")]);
        assert!(len > 0);
        assert_eq!(
            &output[..len as usize],
            br#"{"decision":"consume","messages":[{"actor":"session-switcher","payload":[123,34,116,121,112,101,34,58,34,115,105,100,101,98,97,114,45,97,99,116,105,111,110,34,44,34,97,99,116,105,111,110,34,58,34,116,111,103,103,108,101,34,125]}]}"#
        );
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
