#![no_std]

use core::{panic::PanicInfo, ptr, slice};

const HEAP_BYTES: usize = 256 * 1024;
const RESPONSE_BYTES: usize = 64 * 1024;
const OUTBOX_BYTES: usize = 64 * 1024;
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

#[link(wasm_import_module = "manifold")]
unsafe extern "C" {
    fn read_message() -> i32;
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
    let state = guest_slice(state_ptr, state_len).unwrap_or(&[]);
    let output = response_buffer_mut();
    let input = KeyInput::parse(query);

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
            write_consume(output, messages_for_global_key(input.key_code, state, &mut buffer))
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
pub extern "C" fn manifold_actor_workspace_epoch_run(_epoch: i64, inbox_ptr: i32, _inbox_cap: i32) -> i64 {
    let message_len = unsafe { read_message() };
    if message_len <= 0 {
        return 0;
    }
    let Some(message) = guest_slice(inbox_ptr, message_len) else {
        return -2;
    };
    if find_after(message, br#""type":"layout-state""#).is_none() {
        return 0;
    }
    let output = outbox_buffer_mut();
    let mut panes = [0_u64; 64];
    let pane_count = parse_visible_panes(message, &mut panes);
    if pane_count == 0 {
        return 0;
    }
    let focus = parse_optional_u64_field(message, b"\"focused_pane\":");
    let len = write_layout_actor_outbox(output, message, &panes[..pane_count], focus);
    if len <= 0 {
        return -2;
    }
    pack_actor_outbox(output.as_ptr() as i32, len)
}

#[no_mangle]
pub extern "C" fn manifold_actor_workspace_subscribe_layout_state() {}

fn messages_for_global_key<'a>(
    key_code: i32,
    state: &[u8],
    buffer: &'a mut [Message<'static>; 1],
) -> &'a [Message<'static>] {
    match key_code {
        KEY_J => &[Message::LayoutAction("focus-next")],
        KEY_K => &[Message::LayoutAction("focus-previous")],
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
    state: &[u8],
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
    state: &[u8],
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

    let focused = parse_optional_u64_field(state, b"\"focused_pane\":");
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

fn is_global_workspace_key(key_code: i32) -> bool {
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
}

fn write_layout_actor_outbox(
    output: &mut [u8],
    layout_message: &[u8],
    panes: &[u64],
    focus: Option<u64>,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":[{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    write_set_resolved_layout_payload(&mut payload, panes, focus);
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
    write_app_pane(&mut publication, layout_message, b"pi-foot", &mut first_app_pane);
    write_app_pane(
        &mut publication,
        layout_message,
        b"manifold-terminal-1",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        layout_message,
        b"manifold-terminal-2",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        layout_message,
        b"manifold-terminal-3",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        layout_message,
        b"manifold-terminal-4",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        layout_message,
        b"manifold-terminal-5",
        &mut first_app_pane,
    );
    publication.bytes(br#"]}"#);
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

fn write_app_pane(
    publication: &mut PayloadWriter<'_, '_>,
    layout_message: &[u8],
    app_id: &[u8],
    first: &mut bool,
) {
    let Some(pane) = pane_for_app_id_in_layout_message(layout_message, app_id) else {
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
) {
    payload.bytes(br#"{"type":"set-resolved-layout","layout":{"items":["#);
    let pane_count = panes.len();
    if pane_count == 1 {
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
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"decision":"consume","messages":["#);
    let mut index = 0;
    while index < messages.len() {
        if index > 0 {
            writer.byte(b',');
        }
        writer.bytes(br#"{"actor":"host","payload":["#);
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
        }
        writer.bytes(br#"]}"#);
        index += 1;
    }
    writer.bytes(br#"]}"#);
    finish_response(writer.finish())
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

struct KeyInput {
    key_code: i32,
    key_state: i32,
    modifiers: i32,
}

impl KeyInput {
    fn parse(bytes: &[u8]) -> Self {
        Self {
            key_code: parse_i32_field(bytes, b"\"key_code\":").unwrap_or(0),
            key_state: parse_i32_field(bytes, b"\"key_state\":").unwrap_or(0),
            modifiers: parse_i32_field(bytes, b"\"modifiers\":").unwrap_or(0),
        }
    }
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

fn pane_for_app_id(state: &[u8], app_id: &[u8]) -> Option<u64> {
    pane_for_app_id_in_list(state, b"\"app_panes\":[", app_id)
}

fn pane_for_app_id_in_layout_message(message: &[u8], app_id: &[u8]) -> Option<u64> {
    pane_for_app_id_in_list(message, b"\"pane_app_ids\":[", app_id)
}

fn pane_for_app_id_in_list(bytes: &[u8], list_needle: &[u8], app_id: &[u8]) -> Option<u64> {
    let mut cursor = find_after(bytes, list_needle)?;
    loop {
        cursor = skip_ws(bytes, cursor);
        if cursor >= bytes.len() || bytes[cursor] == b']' {
            return None;
        }
        let object_end = find_after(&bytes[cursor..], b"}")? + cursor;
        let object = &bytes[cursor..object_end];
        let pane = parse_optional_u64_field(object, b"\"pane\":");
        if pane.is_some() && object_has_string_field(object, b"\"app_id\":", app_id) {
            return pane;
        }
        cursor = object_end;
        if cursor < bytes.len() && bytes[cursor] == b',' {
            cursor += 1;
        }
    }
}

fn object_has_string_field(object: &[u8], needle: &[u8], expected: &[u8]) -> bool {
    let Some(cursor) = find_after(object, needle) else {
        return false;
    };
    let cursor = skip_ws(object, cursor);
    if object.get(cursor).copied() != Some(b'"') {
        return false;
    }
    let value_start = cursor + 1;
    let value_end = value_start + expected.len();
    object.get(value_start..value_end) == Some(expected)
        && object.get(value_end).copied() == Some(b'"')
}

fn parse_visible_panes(bytes: &[u8], output: &mut [u64]) -> usize {
    let Some(mut cursor) = find_after(bytes, b"\"visible_panes\":[") else {
        return 0;
    };
    let mut len = 0;
    loop {
        cursor = skip_ws(bytes, cursor);
        if cursor >= bytes.len() || bytes[cursor] == b']' || len >= output.len() {
            return len;
        }
        let (value, next) = parse_u64_at(bytes, cursor);
        if next == cursor {
            return len;
        }
        output[len] = value;
        len += 1;
        cursor = skip_ws(bytes, next);
        if cursor < bytes.len() && bytes[cursor] == b',' {
            cursor += 1;
        }
    }
}

fn parse_i32_field(bytes: &[u8], needle: &[u8]) -> Option<i32> {
    let cursor = find_after(bytes, needle)?;
    let cursor = skip_ws(bytes, cursor);
    let negative = bytes.get(cursor).copied() == Some(b'-');
    let cursor = if negative { cursor + 1 } else { cursor };
    let (value, next) = parse_u64_at(bytes, cursor);
    if next == cursor {
        return None;
    }
    Some(if negative {
        -(value as i32)
    } else {
        value as i32
    })
}

fn parse_optional_u64_field(bytes: &[u8], needle: &[u8]) -> Option<u64> {
    let cursor = skip_ws(bytes, find_after(bytes, needle)?);
    if bytes.get(cursor).copied() == Some(b'n') {
        return None;
    }
    let (value, next) = parse_u64_at(bytes, cursor);
    (next != cursor).then_some(value)
}

fn parse_u64_at(bytes: &[u8], mut cursor: usize) -> (u64, usize) {
    let mut value = 0_u64;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add((bytes[cursor] - b'0') as u64);
        cursor += 1;
    }
    (value, cursor)
}

fn find_after(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            return Some(index + needle.len());
        }
        index += 1;
    }
    None
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\n' | b'\r' | b'\t') {
        cursor += 1;
    }
    cursor
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

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
