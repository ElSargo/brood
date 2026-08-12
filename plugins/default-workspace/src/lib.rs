#![no_std]

#[cfg(test)]
extern crate std;

use brood_keybindings::{Action, Binding, ACTIONS};
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::{ptr, slice};
use serde::Deserialize;
use serde_json_core::heapless::{String as FixedString, Vec as FixedVec};

mod backend_layout;

use backend_layout::pane_placement;

const HEAP_BYTES: usize = 256 * 1024;
const RESPONSE_BYTES: usize = 64 * 1024;
const OUTBOX_BYTES: usize = 64 * 1024;
const MAX_PANES: usize = 64;
const MAX_APP_ID_BYTES: usize = 256;
const MAX_ACTION_ID_BYTES: usize = 64;
const MAX_BINDINGS: usize = 64;
const MAX_MESSAGE_KIND_BYTES: usize = 64;
const MAX_POINTER_PHASE_BYTES: usize = 32;
const JSON_UNESCAPE_BYTES: usize = 512;
const OUTPUT_ERROR: i32 = -1;
const OUTPUT_ABSTAIN: i32 = 0;
const OUTPUT_OK: i32 = 1;

const KEY_RELEASED: i32 = 0;
const KEY_PRESSED: i32 = 1;
const INPUT_MODE_PASSTHROUGH: i32 = 0;
const INPUT_MODE_NORMAL: i32 = 1;
const PRIMARY_BUTTON: u32 = 0x110;
// Stable lease-local chrome IDs near the middle of the u64 space in an ASCII
// "BROOD" namespace. Managed pane IDs grow up from the low end while plugin
// pane IDs grow down from `u64::MAX`, so chrome must use neither allocator end.
const DIVIDER_ZONE_ID: u64 = 0x8000_4252_4f4f_4401;
const DIVIDER_ITEM_ID: u64 = 0x8000_4252_4f4f_4501;
const DIVIDER_EVENT_MASK: u8 = 0b1111;
const MIN_PANE_SIZE_PIXELS: f32 = 96.0;
const MAX_LAYOUT_BOUNDARIES: usize = MAX_PANES * 2;
const MAX_PENDING_RESIZE_PROPOSALS: usize = 256;

static mut HEAP: [u8; HEAP_BYTES] = [0; HEAP_BYTES];
static mut HEAP_CURSOR: usize = 0;
static mut RESPONSE: [u8; RESPONSE_BYTES] = [0; RESPONSE_BYTES];
static mut RESPONSE_DESC: [i32; 4] = [0; 4];
static mut OUTBOX: [u8; OUTBOX_BYTES] = [0; OUTBOX_BYTES];
static mut WORKSPACE_INPUT_MODE: i32 = INPUT_MODE_PASSTHROUGH;
static mut LAYOUT_PANES: [u64; 64] = [0; 64];
static mut LAYOUT_PANE_BACKENDS: [u64; 64] = [0; 64];
static mut LAYOUT_PANE_COUNT: usize = 0;
static mut LAYOUT_FOCUS: u64 = 0;
static mut LAYOUT_HAS_FOCUS: bool = false;
static mut LAYOUT_FULLSCREEN: bool = false;
static mut LAYOUT_LEASE_ID: u64 = 0;
static mut LAYOUT_LEASE_GENERATION: u64 = 0;
static mut LAYOUT_REVISION: u64 = 0;
static mut LAYOUT_BASE_PRESENTED_REVISION: u64 = 0;
static mut LAYOUT_HAS_LEASE: bool = false;
static mut LAYOUT_VIEWPORT_WIDTH: i32 = 1280;
static mut LAYOUT_VIEWPORT_HEIGHT: i32 = 800;
static mut LAYOUT_WAITABLE_PANES: [u64; 64] = [0; 64];
static mut LAYOUT_WAITABLE_PANE_COUNT: usize = 0;
// Pointer motion updates requested geometry immediately so later samples in
// the same drag stay continuous. Ordinary layout publications use durable
// geometry until the host confirms that a synchronized request was presented.
static mut REQUESTED_LAYOUT: [LayoutTransform; MAX_PANES] = [LayoutTransform::EMPTY; MAX_PANES];
static mut DURABLE_LAYOUT: [LayoutTransform; MAX_PANES] = [LayoutTransform::EMPTY; MAX_PANES];
static mut LAYOUT_GEOMETRY_PANES: [u64; MAX_PANES] = [0; MAX_PANES];
static mut LAYOUT_GEOMETRY_BACKENDS: [u64; MAX_PANES] = [0; MAX_PANES];
static mut LAYOUT_GEOMETRY_PANE_COUNT: usize = 0;
static mut LAYOUT_GEOMETRY_VIEWPORT_WIDTH: i32 = 0;
static mut LAYOUT_GEOMETRY_VIEWPORT_HEIGHT: i32 = 0;
static mut LAYOUT_GEOMETRY_CUSTOMIZED: bool = false;
static mut PENDING_RESIZE_REQUEST_IDS: [u64; MAX_PENDING_RESIZE_PROPOSALS] =
    [0; MAX_PENDING_RESIZE_PROPOSALS];
static mut PENDING_RESIZE_AXES: [u8; MAX_PENDING_RESIZE_PROPOSALS] =
    [0; MAX_PENDING_RESIZE_PROPOSALS];
static mut PENDING_RESIZE_NEGATIVE_MASKS: [u64; MAX_PENDING_RESIZE_PROPOSALS] =
    [0; MAX_PENDING_RESIZE_PROPOSALS];
static mut PENDING_RESIZE_POSITIVE_MASKS: [u64; MAX_PENDING_RESIZE_PROPOSALS] =
    [0; MAX_PENDING_RESIZE_PROPOSALS];
static mut PENDING_RESIZE_COORDINATES: [f32; MAX_PENDING_RESIZE_PROPOSALS] =
    [0.0; MAX_PENDING_RESIZE_PROPOSALS];
static mut ACTIVE_POINTER_INTERACTION: u64 = 0;
static mut HAS_ACTIVE_POINTER_INTERACTION: bool = false;
static mut LAST_POINTER_EVENT_SEQUENCE: u64 = 0;
static mut ACTIVE_POINTER_ZONE: u64 = 0;
static mut ACTIVE_BOUNDARY_AXIS: u8 = 0;
static mut ACTIVE_BOUNDARY_NEGATIVE_MASK: u64 = 0;
static mut ACTIVE_BOUNDARY_POSITIVE_MASK: u64 = 0;
static mut POINTER_BOUNDARY_OFFSET: f32 = 0.0;
static mut NEXT_LAYOUT_REQUEST_ID: u64 = 1;
static mut CONSUMED_KEYS: [u64; 4] = [0; 4];

type PaneList = FixedVec<u64, MAX_PANES>;
type AppPaneList = FixedVec<AppPane, MAX_PANES>;
type LayoutActorState = (
    &'static [u64],
    &'static [u64],
    Option<u64>,
    bool,
    Option<LayoutLeaseToken>,
    Option<u64>,
);

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
    #[serde(default)]
    bindings: FixedVec<BindingOverride, MAX_BINDINGS>,
}

#[derive(Deserialize)]
struct AppPane {
    pane: u64,
    app_id: FixedString<MAX_APP_ID_BYTES>,
}

#[derive(Deserialize)]
struct BindingOverride {
    action: FixedString<MAX_ACTION_ID_BYTES>,
    key_code: i32,
    modifiers: i32,
}

#[derive(Default, Deserialize)]
struct WorkspaceMessage {
    #[serde(rename = "type")]
    message_type: FixedString<MAX_MESSAGE_KIND_BYTES>,
    #[serde(default)]
    action: Option<FixedString<MAX_MESSAGE_KIND_BYTES>>,
    #[serde(default)]
    request_id: Option<u64>,
    #[serde(default)]
    resize_epoch: Option<LayoutNotificationEpoch>,
    #[serde(default)]
    visible_panes: PaneList,
    #[serde(default)]
    pane_realms: FixedVec<PaneRealm, MAX_PANES>,
    #[serde(default)]
    focused_pane: Option<u64>,
    #[serde(default)]
    pane_app_ids: AppPaneList,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    inventory_revision: Option<u64>,
    #[serde(default)]
    base_layout_revision: Option<u64>,
    #[serde(default)]
    presented_layout_revision: Option<u64>,
    #[serde(default)]
    layout_revision: Option<u64>,
    #[serde(default)]
    native_resize_revision: Option<u64>,
    #[serde(default)]
    lease: Option<LayoutLeaseToken>,
    #[serde(default)]
    size: LayoutStateSize,
    #[serde(default, alias = "waitable_plugin_panes")]
    waitable_panes: PaneList,
    #[serde(default)]
    interaction: Option<u64>,
    #[serde(default)]
    event_sequence: Option<u64>,
    #[serde(default)]
    zone: Option<u64>,
    #[serde(default)]
    phase: Option<FixedString<MAX_POINTER_PHASE_BYTES>>,
    #[serde(default)]
    position: Option<LayoutPoint>,
    #[serde(default)]
    button: Option<u32>,
    #[serde(default, rename = "modifiers")]
    _modifiers: u32,
}

#[derive(Clone, Copy, Deserialize)]
struct PaneRealm {
    pane: u64,
    backend: u64,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
struct LayoutLeaseToken {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct LayoutStateSize {
    w: i32,
    h: i32,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct LayoutPoint {
    u: f64,
    v: f64,
}

#[derive(Clone, Copy, Deserialize)]
struct LayoutNotificationEpoch {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    lease: u64,
    #[serde(default)]
    lease_generation: u64,
    #[serde(default)]
    base_layout_revision: u64,
    target_layout_revision: u64,
}

#[derive(Clone, Copy)]
struct SynchronizedRequestContext {
    focus: Option<u64>,
    fullscreen: bool,
    lease: LayoutLeaseToken,
    interaction: Option<u64>,
    event_sequence: Option<u64>,
    request_id: u64,
    native_resize_revision: Option<u64>,
    use_requested_geometry: bool,
}

#[derive(Clone, Copy)]
struct LayoutTransform {
    offset_u: f32,
    offset_v: f32,
    size_u: f32,
    size_v: f32,
}

impl LayoutTransform {
    const EMPTY: Self = Self {
        offset_u: 0.0,
        offset_v: 0.0,
        size_u: 0.0,
        size_v: 0.0,
    };
}

#[derive(Clone, Copy)]
struct LayoutBoundary {
    axis: u8,
    fixed: f32,
    start: f32,
    end: f32,
    negative_mask: u64,
    positive_mask: u64,
}

impl LayoutBoundary {
    const EMPTY: Self = Self {
        axis: 0,
        fixed: 0.0,
        start: 0.0,
        end: 0.0,
        negative_mask: 0,
        positive_mask: 0,
    };
}

#[derive(Deserialize)]
struct PointerPolicyQuery {
    phase: FixedString<MAX_POINTER_PHASE_BYTES>,
    #[serde(default)]
    button: Option<u32>,
    captured: bool,
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
#[link(wasm_import_module = "brood")]
unsafe extern "C" {
    fn read_message() -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn read_message() -> i32 {
    0
}

#[no_mangle]
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
pub extern "C" fn brood_arena_free(_ptr: i32, _capacity: i32, _align: i32) {}

#[no_mangle]
pub extern "C" fn brood_policy_provider_key_workspace(
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

    if input.key_state == KEY_RELEASED {
        return if take_consumed_key(input.key_code) {
            write_consume(output, &[])
        } else {
            OUTPUT_ABSTAIN
        };
    }
    if input.key_state != KEY_PRESSED {
        return OUTPUT_ABSTAIN;
    }

    if focused_keybinding_editor(&state) {
        return OUTPUT_ABSTAIN;
    }
    if input.modifiers & brood_keybindings::MOD_CTRL != 0
        && input.modifiers & brood_keybindings::MOD_ALT == 0
        && focused_raw_control_plugin(&state)
    {
        return OUTPUT_ABSTAIN;
    }

    let normal_mode = workspace_input_mode() == INPUT_MODE_NORMAL;
    let Some(action) = matching_action(&state, &input, normal_mode) else {
        return OUTPUT_ABSTAIN;
    };
    mark_consumed_key(input.key_code);
    if input.repeat {
        return write_consume(output, &[]);
    }
    if action.id == "keybindings.open" {
        return write_keybindings_show(output, &state.bindings);
    }
    if action.id == "workspace.mode" {
        toggle_workspace_input_mode();
        return write_consume(output, &[]);
    }

    let mut buffer = [Message::Noop; 2];
    write_consume(output, messages_for_action(action, &state, &mut buffer))
}

#[no_mangle]
pub extern "C" fn brood_policy_provider_pointer_workspace(
    query_ptr: i32,
    query_len: i32,
    _state_ptr: i32,
    _state_len: i32,
) -> i32 {
    let Some(query) = guest_slice(query_ptr, query_len) else {
        return OUTPUT_ERROR;
    };
    let Some(query) = decode_json::<PointerPolicyQuery>(query) else {
        return OUTPUT_ERROR;
    };
    let Some(capture) = pointer_capture_decision(&query) else {
        return OUTPUT_ABSTAIN;
    };
    finish_response(write_pointer_policy_result(
        response_buffer_mut(),
        capture,
        "lease-holder",
    ))
}

fn pointer_capture_decision(query: &PointerPolicyQuery) -> Option<&'static str> {
    let capture = match query.phase.as_str() {
        "button-press" if !query.captured && query.button == Some(PRIMARY_BUTTON) => "acquire",
        "motion" if query.captured => "unchanged",
        "button-release" if query.captured && query.button == Some(PRIMARY_BUTTON) => "release",
        "cancel" if query.captured => "release",
        _ => return None,
    };
    Some(capture)
}

#[no_mangle]
pub extern "C" fn brood_actor_workspace_epoch_run(
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
            &message.pane_realms,
            message.focused_pane,
            message.lease,
            message.inventory_revision.or(message.revision),
            message
                .base_layout_revision
                .or(message.presented_layout_revision)
                .or(message.layout_revision),
            &message.waitable_panes,
            message.size,
        );
        let (panes, pane_backends, focus, fullscreen, lease, revision) = layout_actor_state();
        let len = if let (Some(native_resize_revision), Some(lease)) = (
            message
                .native_resize_revision
                .filter(|revision| *revision != 0),
            lease,
        ) {
            let request_id = next_layout_request_id();
            write_synchronized_layout_outbox(
                output,
                panes,
                pane_backends,
                SynchronizedRequestContext {
                    focus,
                    fullscreen,
                    lease,
                    interaction: None,
                    event_sequence: None,
                    request_id,
                    native_resize_revision: Some(native_resize_revision),
                    use_requested_geometry: false,
                },
            )
        } else {
            write_layout_actor_outbox(
                output,
                &message.pane_app_ids,
                panes,
                pane_backends,
                focus,
                fullscreen,
                lease,
                revision,
            )
        };
        if len <= 0 {
            return -2;
        }
        return pack_actor_outbox(output.as_ptr() as i32, len);
    }

    if message.message_type == "layout-region-resized" {
        let Some(epoch) = message.resize_epoch else {
            return 0;
        };
        let Some(lease) = message.lease else {
            return 0;
        };
        let (panes, pane_backends, focus, fullscreen, current_lease, revision) =
            layout_actor_state();
        if current_lease != Some(lease)
            || epoch.id == 0
            || epoch.lease == 0
            || epoch.lease_generation == 0
            || revision.is_none()
        {
            return 0;
        }
        let old_size = unsafe { (LAYOUT_VIEWPORT_WIDTH, LAYOUT_VIEWPORT_HEIGHT) };
        unsafe {
            LAYOUT_VIEWPORT_WIDTH = message.size.w.max(1);
            LAYOUT_VIEWPORT_HEIGHT = message.size.h.max(1);
        }
        let len = write_layout_region_outbox(
            output,
            panes,
            pane_backends,
            focus,
            fullscreen,
            lease,
            revision.unwrap_or(0),
            epoch,
        );
        unsafe {
            LAYOUT_VIEWPORT_WIDTH = old_size.0;
            LAYOUT_VIEWPORT_HEIGHT = old_size.1;
        }
        if len <= 0 {
            return -2;
        }
        return pack_actor_outbox(output.as_ptr() as i32, len);
    }

    if message.message_type == "layout-pointer" {
        let len = handle_layout_pointer(output, &message);
        if len < 0 {
            return -2;
        }
        return if len == 0 {
            0
        } else {
            pack_actor_outbox(output.as_ptr() as i32, len)
        };
    }

    if handle_synchronized_layout_notification(&message) {
        return 0;
    }

    if message.message_type != "layout-action" {
        return 0;
    }
    apply_layout_actor_action(message.action.as_deref().unwrap_or(""));
    let (panes, pane_backends, focus, fullscreen, lease, revision) = layout_actor_state();
    let len = write_layout_actor_update_outbox(
        output,
        panes,
        pane_backends,
        focus,
        fullscreen,
        lease,
        revision,
    );
    if len <= 0 {
        return -2;
    }
    pack_actor_outbox(output.as_ptr() as i32, len)
}

#[no_mangle]
pub extern "C" fn brood_actor_workspace_subscribe_layout_state() {}

fn messages_for_action<'a>(
    action: &Action,
    state: &PolicyState,
    buffer: &'a mut [Message<'static>; 2],
) -> &'a [Message<'static>] {
    match action.id {
        "shortcuts.passthrough" => &[Message::ToggleShortcutPassthrough],
        "agent.picker" => &[Message::CodingAgentAction("picker")],
        "pane.close" => &[Message::LayoutAction("close-active-pane")],
        "terminal.new" => &[Message::TerminalAction("new")],
        "editor.open" => &[Message::EditorAction("open")],
        "inbox.show" => &[Message::InboxAction("show")],
        "sessions.toggle" => &[Message::SessionSwitcherAction("toggle")],
        "sessions.picker" => &[Message::SessionSwitcherAction("picker")],
        "diff.toggle" => &[Message::DiffViewerAction("toggle")],
        "sessions.next" => &[Message::SwitchActiveSessionNext],
        "focus.next" => focus_cycle_managed_pane(state, true, buffer),
        "focus.previous" => focus_cycle_managed_pane(state, false, buffer),
        "terminal.spawn" => &[Message::SpawnApp(&["foot"])],
        "split.down" => &[Message::LayoutAction("split-active-down")],
        "split.right" => &[Message::LayoutAction("split-active-right")],
        "split.smart" => &[Message::LayoutAction("split-active-smart")],
        "split.join" => &[Message::LayoutAction("join-active-pane")],
        "split.next" => focus_cycle_terminal(state, true, buffer)
            .unwrap_or(&[Message::LayoutAction("focus-next-split")]),
        "split.previous" => focus_cycle_terminal(state, false, buffer)
            .unwrap_or(&[Message::LayoutAction("focus-previous-split")]),
        "lazygit.open" => &[Message::FocusOrSpawnApp {
            app_id: "brood-lazygit",
            argv: &["foot", "--app-id", "brood-lazygit", "lazygit"],
        }],
        "pi.open" => focus_or_spawn_app(
            state,
            b"pi-foot",
            &["foot", "--app-id", "pi-foot", "pi"],
            buffer,
        ),
        "terminal.1" => focus_or_spawn_app(
            state,
            b"brood-terminal-1",
            &["foot", "--app-id", "brood-terminal-1"],
            buffer,
        ),
        "terminal.2" => focus_or_spawn_app(
            state,
            b"brood-terminal-2",
            &["foot", "--app-id", "brood-terminal-2"],
            buffer,
        ),
        "terminal.3" => focus_or_spawn_app(
            state,
            b"brood-terminal-3",
            &["foot", "--app-id", "brood-terminal-3"],
            buffer,
        ),
        "terminal.4" => focus_or_spawn_app(
            state,
            b"brood-terminal-4",
            &["foot", "--app-id", "brood-terminal-4"],
            buffer,
        ),
        "terminal.5" => focus_or_spawn_app(
            state,
            b"brood-terminal-5",
            &["foot", "--app-id", "brood-terminal-5"],
            buffer,
        ),
        "session.new" => &[Message::CreateAndAttachSession],
        "wezterm.spawn" => &[Message::SpawnApp(&[
            "flatpak",
            "run",
            "--env=GDK_BACKEND=wayland",
            "--env=QT_QPA_PLATFORM=wayland",
            "org.wezfurlong.wezterm",
        ])],
        "pane.close-workspace" => &[Message::LayoutAction("close-active-pane")],
        "pane.fullscreen" => &[Message::LayoutAction("toggle-managed-fullscreen")],
        "dock.bottom" => &[Message::LayoutAction("toggle-bottom-dock")],
        "dock.sides" => {
            buffer[0] = Message::LayoutAction("toggle-left-dock");
            buffer[1] = Message::LayoutAction("toggle-right-dock");
            buffer
        }
        _ => &[],
    }
}

fn focus_or_spawn_app<'a>(
    state: &PolicyState,
    app_id: &'static [u8],
    argv: &'static [&'static str],
    buffer: &'a mut [Message<'static>; 2],
) -> &'a [Message<'static>] {
    if let Some(pane) = pane_for_app_id(state, app_id) {
        buffer[0] = Message::FocusPane(pane);
    } else {
        buffer[0] = Message::SpawnApp(argv);
    }
    &buffer[..1]
}

fn focus_cycle_terminal<'a>(
    state: &PolicyState,
    next: bool,
    buffer: &'a mut [Message<'static>; 2],
) -> Option<&'a [Message<'static>]> {
    let mut panes = [0_u64; 5];
    let mut count = 0;
    for app_id in [
        b"brood-terminal-1" as &[u8],
        b"brood-terminal-2" as &[u8],
        b"brood-terminal-3" as &[u8],
        b"brood-terminal-4" as &[u8],
        b"brood-terminal-5" as &[u8],
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
    Some(&buffer[..1])
}

fn focus_cycle_managed_pane<'a>(
    state: &PolicyState,
    next: bool,
    buffer: &'a mut [Message<'static>; 2],
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
    &buffer[..1]
}

fn matching_action(
    state: &PolicyState,
    input: &KeyInput,
    normal_mode: bool,
) -> Option<&'static Action> {
    ACTIONS.iter().find(|action| {
        action.is_enabled(normal_mode)
            && effective_binding(state, action) == Binding::new(input.key_code, input.modifiers)
    })
}

fn effective_binding(state: &PolicyState, action: &Action) -> Binding {
    state
        .bindings
        .iter()
        .find(|binding| binding.action.as_str() == action.id)
        .map_or(action.default_binding, |binding| {
            Binding::new(binding.key_code, binding.modifiers)
        })
}

fn mark_consumed_key(key_code: i32) {
    let Ok(key_code) = usize::try_from(key_code) else {
        return;
    };
    if key_code >= 256 {
        return;
    }
    unsafe {
        let slot = ptr::addr_of_mut!(CONSUMED_KEYS)
            .cast::<u64>()
            .add(key_code / 64);
        slot.write(slot.read() | (1_u64 << (key_code % 64)));
    }
}

fn take_consumed_key(key_code: i32) -> bool {
    let Ok(key_code) = usize::try_from(key_code) else {
        return false;
    };
    if key_code >= 256 {
        return false;
    }
    unsafe {
        let slot = ptr::addr_of_mut!(CONSUMED_KEYS)
            .cast::<u64>()
            .add(key_code / 64);
        let mask = 1_u64 << (key_code % 64);
        let consumed = slot.read() & mask != 0;
        slot.write(slot.read() & !mask);
        consumed
    }
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
    CodingAgentAction(&'a str),
    DiffViewerAction(&'a str),
    InboxAction(&'a str),
    TerminalAction(&'a str),
    EditorAction(&'a str),
}

#[allow(
    clippy::too_many_arguments,
    reason = "the state publication and initial layout share one bounded outbox"
)]
fn write_layout_actor_outbox(
    output: &mut [u8],
    app_panes: &[AppPane],
    panes: &[u64],
    pane_backends: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":["#);
    if let Some(lease) = lease {
        writer.bytes(br#"{"actor":"host","payload":["#);
        let mut payload = PayloadWriter::new(&mut writer);
        write_set_pointer_policy_payload(&mut payload, lease);
        writer.bytes(br#"]},"#);
    }
    writer.bytes(br#"{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    write_set_resolved_layout_payload(
        &mut payload,
        panes,
        pane_backends,
        focus,
        fullscreen,
        lease,
        revision,
    );
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
    publication.bytes(br#", "focus_rules":[{"key":36,"modifiers":1,"app_id":"pi-foot","argv":["foot","--app-id","pi-foot","pi"]},{"key":10,"modifiers":1,"app_id":"brood-terminal-1","argv":["foot","--app-id","brood-terminal-1"]},{"key":11,"modifiers":1,"app_id":"brood-terminal-2","argv":["foot","--app-id","brood-terminal-2"]},{"key":12,"modifiers":1,"app_id":"brood-terminal-3","argv":["foot","--app-id","brood-terminal-3"]},{"key":13,"modifiers":1,"app_id":"brood-terminal-4","argv":["foot","--app-id","brood-terminal-4"]},{"key":14,"modifiers":1,"app_id":"brood-terminal-5","argv":["foot","--app-id","brood-terminal-5"]}], "panes":["#);
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
        b"brood-terminal-1",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"brood-terminal-2",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"brood-terminal-3",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"brood-terminal-4",
        &mut first_app_pane,
    );
    write_app_pane(
        &mut publication,
        app_panes,
        b"brood-terminal-5",
        &mut first_app_pane,
    );
    publication.bytes(br#"]}"#);
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

fn write_layout_actor_update_outbox(
    output: &mut [u8],
    panes: &[u64],
    pane_backends: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":["#);
    if let Some(lease) = lease {
        writer.bytes(br#"{"actor":"host","payload":["#);
        let mut payload = PayloadWriter::new(&mut writer);
        write_set_pointer_policy_payload(&mut payload, lease);
        writer.bytes(br#"]},"#);
    }
    writer.bytes(br#"{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    write_set_resolved_layout_payload(
        &mut payload,
        panes,
        pane_backends,
        focus,
        fullscreen,
        lease,
        revision,
    );
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the host layout-state fields are copied into durable fixed storage together"
)]
fn store_layout_actor_state(
    panes: &[u64],
    pane_realms: &[PaneRealm],
    focus: Option<u64>,
    lease: Option<LayoutLeaseToken>,
    revision: Option<u64>,
    base_layout_revision: Option<u64>,
    waitable_panes: &[u64],
    size: LayoutStateSize,
) {
    let pane_count = panes.len().min(64);
    let width = size.w.max(1);
    let height = size.h.max(1);
    let pane_backends = pane_realms_to_backends(&panes[..pane_count], pane_realms);
    let topology_changed = !layout_geometry_topology_matches(&panes[..pane_count], &pane_backends);
    let viewport_changed = unsafe {
        LAYOUT_GEOMETRY_VIEWPORT_WIDTH != width || LAYOUT_GEOMETRY_VIEWPORT_HEIGHT != height
    };
    unsafe {
        let target = ptr::addr_of_mut!(LAYOUT_PANES).cast::<u64>();
        let mut index = 0;
        while index < pane_count {
            target.add(index).write(panes[index]);
            let backend = pane_realms
                .iter()
                .find(|realm| realm.pane == panes[index])
                .map_or(0, |realm| realm.backend);
            ptr::addr_of_mut!(LAYOUT_PANE_BACKENDS)
                .cast::<u64>()
                .add(index)
                .write(backend);
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
            LAYOUT_BASE_PRESENTED_REVISION = base_layout_revision.unwrap_or(revision);
            LAYOUT_HAS_LEASE = true;
        } else {
            LAYOUT_HAS_LEASE = false;
        }
        LAYOUT_VIEWPORT_WIDTH = width;
        LAYOUT_VIEWPORT_HEIGHT = height;
        let waitable_target = ptr::addr_of_mut!(LAYOUT_WAITABLE_PANES).cast::<u64>();
        let mut waitable_count = 0;
        for pane in waitable_panes {
            if waitable_count >= MAX_PANES
                || !panes[..pane_count].contains(pane)
                || slice::from_raw_parts(waitable_target, waitable_count).contains(pane)
            {
                continue;
            }
            waitable_target.add(waitable_count).write(*pane);
            waitable_count += 1;
        }
        LAYOUT_WAITABLE_PANE_COUNT = waitable_count;
    }
    let preserve_custom_geometry = unsafe { LAYOUT_GEOMETRY_CUSTOMIZED } && !topology_changed;
    if topology_changed || (viewport_changed && !preserve_custom_geometry) {
        let (_, pane_backends, _, _, _, _) = layout_actor_state();
        reset_layout_geometry(&panes[..pane_count], pane_backends, width, height);
    } else if viewport_changed {
        unsafe {
            LAYOUT_GEOMETRY_VIEWPORT_WIDTH = width;
            LAYOUT_GEOMETRY_VIEWPORT_HEIGHT = height;
        }
        clear_resize_proposals();
        rebuild_requested_layout();
        clear_pointer_interaction();
    }
}

fn pane_realms_to_backends(panes: &[u64], pane_realms: &[PaneRealm]) -> FixedVec<u64, MAX_PANES> {
    let mut backends = FixedVec::new();
    for pane in panes {
        let backend = pane_realms
            .iter()
            .find(|realm| realm.pane == *pane)
            .map_or(0, |realm| realm.backend);
        let _ = backends.push(backend);
    }
    backends
}

fn layout_actor_state() -> LayoutActorState {
    unsafe {
        let pane_count = LAYOUT_PANE_COUNT.min(64);
        let panes = slice::from_raw_parts(ptr::addr_of!(LAYOUT_PANES).cast::<u64>(), pane_count);
        let pane_backends = slice::from_raw_parts(
            ptr::addr_of!(LAYOUT_PANE_BACKENDS).cast::<u64>(),
            pane_count,
        );
        let focus = LAYOUT_HAS_FOCUS.then_some(LAYOUT_FOCUS);
        let lease = LAYOUT_HAS_LEASE.then_some(LayoutLeaseToken {
            id: LAYOUT_LEASE_ID,
            generation: LAYOUT_LEASE_GENERATION,
        });
        let revision = LAYOUT_HAS_LEASE.then_some(LAYOUT_REVISION);
        (
            panes,
            pane_backends,
            focus,
            LAYOUT_FULLSCREEN,
            lease,
            revision,
        )
    }
}

fn layout_waitable_panes() -> &'static [u64] {
    unsafe {
        slice::from_raw_parts(
            ptr::addr_of!(LAYOUT_WAITABLE_PANES).cast::<u64>(),
            LAYOUT_WAITABLE_PANE_COUNT.min(MAX_PANES),
        )
    }
}

fn layout_geometry_matches(panes: &[u64], pane_backends: &[u64], width: i32, height: i32) -> bool {
    unsafe {
        if LAYOUT_GEOMETRY_VIEWPORT_WIDTH != width || LAYOUT_GEOMETRY_VIEWPORT_HEIGHT != height {
            return false;
        }
    }
    layout_geometry_topology_matches(panes, pane_backends)
}

fn layout_geometry_topology_matches(panes: &[u64], pane_backends: &[u64]) -> bool {
    unsafe {
        if LAYOUT_GEOMETRY_PANE_COUNT != panes.len() {
            return false;
        }
        let geometry_panes = ptr::addr_of!(LAYOUT_GEOMETRY_PANES).cast::<u64>();
        let geometry_backends = ptr::addr_of!(LAYOUT_GEOMETRY_BACKENDS).cast::<u64>();
        for (index, pane) in panes.iter().copied().enumerate() {
            if geometry_panes.add(index).read() != pane
                || geometry_backends.add(index).read()
                    != pane_backends.get(index).copied().unwrap_or(0)
            {
                return false;
            }
        }
    }
    true
}

fn clear_resize_proposals() {
    unsafe {
        let ids = ptr::addr_of_mut!(PENDING_RESIZE_REQUEST_IDS).cast::<u64>();
        for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
            ids.add(index).write(0);
        }
    }
}

fn reset_layout_geometry(panes: &[u64], pane_backends: &[u64], width: i32, height: i32) {
    let pane_count = panes.len().min(MAX_PANES);
    unsafe {
        let requested = ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>();
        let durable = ptr::addr_of_mut!(DURABLE_LAYOUT).cast::<LayoutTransform>();
        let geometry_panes = ptr::addr_of_mut!(LAYOUT_GEOMETRY_PANES).cast::<u64>();
        let geometry_backends = ptr::addr_of_mut!(LAYOUT_GEOMETRY_BACKENDS).cast::<u64>();
        for index in 0..pane_count {
            let transform = initial_layout_transform(index, panes, pane_backends, width, height);
            requested.add(index).write(transform);
            durable.add(index).write(transform);
            geometry_panes.add(index).write(panes[index]);
            geometry_backends
                .add(index)
                .write(pane_backends.get(index).copied().unwrap_or(0));
        }
        LAYOUT_GEOMETRY_PANE_COUNT = pane_count;
        LAYOUT_GEOMETRY_VIEWPORT_WIDTH = width;
        LAYOUT_GEOMETRY_VIEWPORT_HEIGHT = height;
        LAYOUT_GEOMETRY_CUSTOMIZED = false;
    }
    clear_resize_proposals();
    clear_pointer_interaction();
}

fn layout_transform(
    index: usize,
    panes: &[u64],
    pane_backends: &[u64],
    use_requested: bool,
) -> LayoutTransform {
    let (width, height) = unsafe { (LAYOUT_VIEWPORT_WIDTH, LAYOUT_VIEWPORT_HEIGHT) };
    if layout_geometry_matches(panes, pane_backends, width, height) {
        unsafe {
            let source = if use_requested {
                ptr::addr_of!(REQUESTED_LAYOUT).cast::<LayoutTransform>()
            } else {
                ptr::addr_of!(DURABLE_LAYOUT).cast::<LayoutTransform>()
            };
            return source.add(index).read();
        }
    }
    initial_layout_transform(index, panes, pane_backends, width, height)
}

fn initial_layout_transform(
    index: usize,
    panes: &[u64],
    pane_backends: &[u64],
    width: i32,
    height: i32,
) -> LayoutTransform {
    if panes.len() == 2
        && panes
            .iter()
            .all(|pane| layout_waitable_panes().contains(pane))
    {
        return LayoutTransform {
            offset_u: index as f32 - 1.0,
            offset_v: -1.0,
            size_u: 1.0,
            size_v: 2.0,
        };
    }
    let placement = pane_placement(index, panes, pane_backends, width, height);
    LayoutTransform {
        offset_u: placement.offset_u,
        offset_v: placement.offset_v,
        size_u: placement.size_u,
        size_v: placement.size_v,
    }
}

fn add_layout_boundary(
    boundaries: &mut [LayoutBoundary; MAX_LAYOUT_BOUNDARIES],
    count: &mut usize,
    mut boundary: LayoutBoundary,
) {
    const EPSILON: f32 = 0.000_1;
    let mut index = 0;
    while index < *count {
        let existing = boundaries[index];
        let same_line =
            existing.axis == boundary.axis && (existing.fixed - boundary.fixed).abs() <= EPSILON;
        let connected =
            existing.start <= boundary.end + EPSILON && boundary.start <= existing.end + EPSILON;
        if same_line && connected {
            boundary.start = boundary.start.min(existing.start);
            boundary.end = boundary.end.max(existing.end);
            boundary.negative_mask |= existing.negative_mask;
            boundary.positive_mask |= existing.positive_mask;
            *count -= 1;
            boundaries[index] = boundaries[*count];
            index = 0;
            continue;
        }
        index += 1;
    }
    if *count < MAX_LAYOUT_BOUNDARIES {
        boundaries[*count] = boundary;
        *count += 1;
    }
}

fn collect_layout_boundaries(
    panes: &[u64],
    pane_backends: &[u64],
    use_requested: bool,
    boundaries: &mut [LayoutBoundary; MAX_LAYOUT_BOUNDARIES],
) -> usize {
    const EPSILON: f32 = 0.000_1;
    let mut count = 0;
    for first in 0..panes.len() {
        let a = layout_transform(first, panes, pane_backends, use_requested);
        for second in first + 1..panes.len() {
            let b = layout_transform(second, panes, pane_backends, use_requested);
            let vertical_start = a.offset_v.max(b.offset_v);
            let vertical_end = (a.offset_v + a.size_v).min(b.offset_v + b.size_v);
            if vertical_end > vertical_start + EPSILON {
                if (a.offset_u + a.size_u - b.offset_u).abs() <= EPSILON {
                    add_layout_boundary(
                        boundaries,
                        &mut count,
                        LayoutBoundary {
                            axis: 0,
                            fixed: b.offset_u,
                            start: vertical_start,
                            end: vertical_end,
                            negative_mask: 1_u64 << first,
                            positive_mask: 1_u64 << second,
                        },
                    );
                } else if (b.offset_u + b.size_u - a.offset_u).abs() <= EPSILON {
                    add_layout_boundary(
                        boundaries,
                        &mut count,
                        LayoutBoundary {
                            axis: 0,
                            fixed: a.offset_u,
                            start: vertical_start,
                            end: vertical_end,
                            negative_mask: 1_u64 << second,
                            positive_mask: 1_u64 << first,
                        },
                    );
                }
            }
            let horizontal_start = a.offset_u.max(b.offset_u);
            let horizontal_end = (a.offset_u + a.size_u).min(b.offset_u + b.size_u);
            if horizontal_end > horizontal_start + EPSILON {
                if (a.offset_v + a.size_v - b.offset_v).abs() <= EPSILON {
                    add_layout_boundary(
                        boundaries,
                        &mut count,
                        LayoutBoundary {
                            axis: 1,
                            fixed: b.offset_v,
                            start: horizontal_start,
                            end: horizontal_end,
                            negative_mask: 1_u64 << first,
                            positive_mask: 1_u64 << second,
                        },
                    );
                } else if (b.offset_v + b.size_v - a.offset_v).abs() <= EPSILON {
                    add_layout_boundary(
                        boundaries,
                        &mut count,
                        LayoutBoundary {
                            axis: 1,
                            fixed: a.offset_v,
                            start: horizontal_start,
                            end: horizontal_end,
                            negative_mask: 1_u64 << second,
                            positive_mask: 1_u64 << first,
                        },
                    );
                }
            }
        }
    }
    // The zone ID is its index, so keep ordering independent of pair traversal.
    for index in 1..count {
        let boundary = boundaries[index];
        let mut destination = index;
        while destination > 0 {
            let previous = boundaries[destination - 1];
            if (previous.axis, previous.fixed, previous.start)
                <= (boundary.axis, boundary.fixed, boundary.start)
            {
                break;
            }
            boundaries[destination] = previous;
            destination -= 1;
        }
        boundaries[destination] = boundary;
    }
    count
}

fn boundary_is_waitable(boundary: LayoutBoundary, panes: &[u64]) -> bool {
    let affected = boundary.negative_mask | boundary.positive_mask;
    (0..panes.len()).all(|index| {
        affected & (1_u64 << index) == 0 || layout_waitable_panes().contains(&panes[index])
    })
}

const fn divider_zone_id(index: usize) -> u64 {
    DIVIDER_ZONE_ID + index as u64
}

const fn divider_item_id(index: usize) -> u64 {
    DIVIDER_ITEM_ID + index as u64
}

fn apply_boundary(
    target: *mut LayoutTransform,
    pane_count: usize,
    axis: u8,
    negative_mask: u64,
    positive_mask: u64,
    fixed: f32,
) {
    unsafe {
        for index in 0..pane_count {
            let bit = 1_u64 << index;
            let mut transform = target.add(index).read();
            if negative_mask & bit != 0 {
                if axis == 0 {
                    transform.size_u = fixed - transform.offset_u;
                } else {
                    transform.size_v = fixed - transform.offset_v;
                }
            } else if positive_mask & bit != 0 {
                if axis == 0 {
                    let end = transform.offset_u + transform.size_u;
                    transform.offset_u = fixed;
                    transform.size_u = end - fixed;
                } else {
                    let end = transform.offset_v + transform.size_v;
                    transform.offset_v = fixed;
                    transform.size_v = end - fixed;
                }
            }
            target.add(index).write(transform);
        }
    }
}

fn clamped_boundary_coordinate(
    axis: u8,
    negative_mask: u64,
    positive_mask: u64,
    fixed: f32,
) -> f32 {
    let pane_count = unsafe { LAYOUT_GEOMETRY_PANE_COUNT.min(MAX_PANES) };
    let (width, height) = unsafe { (LAYOUT_VIEWPORT_WIDTH.max(1), LAYOUT_VIEWPORT_HEIGHT.max(1)) };
    let minimum = if axis == 0 {
        2.0 * MIN_PANE_SIZE_PIXELS / width as f32
    } else {
        2.0 * MIN_PANE_SIZE_PIXELS / height as f32
    };
    let mut lower: f32 = -1.0;
    let mut upper: f32 = 1.0;
    unsafe {
        let requested = ptr::addr_of!(REQUESTED_LAYOUT).cast::<LayoutTransform>();
        for index in 0..pane_count {
            let transform = requested.add(index).read();
            let bit = 1_u64 << index;
            if negative_mask & bit != 0 {
                let start = if axis == 0 {
                    transform.offset_u
                } else {
                    transform.offset_v
                };
                lower = lower.max(start + minimum);
            }
            if positive_mask & bit != 0 {
                let end = if axis == 0 {
                    transform.offset_u + transform.size_u
                } else {
                    transform.offset_v + transform.size_v
                };
                upper = upper.min(end - minimum);
            }
        }
    }
    if lower <= upper {
        fixed.clamp(lower, upper)
    } else {
        (lower + upper) * 0.5
    }
}

fn remember_resize_proposal(request_id: u64, boundary: LayoutBoundary, fixed: f32) {
    if request_id == 0 {
        return;
    }
    unsafe {
        let ids = ptr::addr_of_mut!(PENDING_RESIZE_REQUEST_IDS).cast::<u64>();
        let mut empty = None;
        let mut oldest = (0, u64::MAX);
        for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
            let candidate = ids.add(index).read();
            if candidate == request_id {
                PENDING_RESIZE_AXES[index] = boundary.axis;
                PENDING_RESIZE_NEGATIVE_MASKS[index] = boundary.negative_mask;
                PENDING_RESIZE_POSITIVE_MASKS[index] = boundary.positive_mask;
                PENDING_RESIZE_COORDINATES[index] = fixed;
                return;
            }
            if candidate == 0 && empty.is_none() {
                empty = Some(index);
            } else if candidate != 0 && candidate < oldest.1 {
                oldest = (index, candidate);
            }
        }
        let slot = empty.unwrap_or(oldest.0);
        ids.add(slot).write(request_id);
        PENDING_RESIZE_AXES[slot] = boundary.axis;
        PENDING_RESIZE_NEGATIVE_MASKS[slot] = boundary.negative_mask;
        PENDING_RESIZE_POSITIVE_MASKS[slot] = boundary.positive_mask;
        PENDING_RESIZE_COORDINATES[slot] = fixed;
    }
}

fn remove_resize_proposal(request_id: u64) {
    unsafe {
        let ids = ptr::addr_of_mut!(PENDING_RESIZE_REQUEST_IDS).cast::<u64>();
        for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
            if ids.add(index).read() == request_id {
                ids.add(index).write(0);
                return;
            }
        }
    }
}

fn remove_resize_proposals_through(request_id: u64) {
    unsafe {
        let ids = ptr::addr_of_mut!(PENDING_RESIZE_REQUEST_IDS).cast::<u64>();
        for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
            let candidate = ids.add(index).read();
            if candidate != 0 && candidate <= request_id {
                ids.add(index).write(0);
            }
        }
    }
}

fn rebuild_requested_layout() {
    unsafe {
        let pane_count = LAYOUT_GEOMETRY_PANE_COUNT.min(MAX_PANES);
        ptr::copy_nonoverlapping(
            ptr::addr_of!(DURABLE_LAYOUT).cast::<LayoutTransform>(),
            ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
            pane_count,
        );
        let ids = ptr::addr_of!(PENDING_RESIZE_REQUEST_IDS).cast::<u64>();
        let mut applied_through = 0;
        loop {
            let mut next = None;
            for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
                let candidate = ids.add(index).read();
                if candidate > applied_through
                    && next.is_none_or(|(_, next_id)| candidate < next_id)
                {
                    next = Some((index, candidate));
                }
            }
            let Some((index, request_id)) = next else {
                break;
            };
            apply_boundary(
                ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
                pane_count,
                PENDING_RESIZE_AXES[index],
                PENDING_RESIZE_NEGATIVE_MASKS[index],
                PENDING_RESIZE_POSITIVE_MASKS[index],
                PENDING_RESIZE_COORDINATES[index],
            );
            applied_through = request_id;
            if request_id == u64::MAX {
                break;
            }
        }
    }
}

fn commit_resize_proposal(request_id: u64) {
    let mut applied_through = 0;
    let mut applied = false;
    loop {
        let mut next = None;
        #[allow(
            clippy::needless_range_loop,
            reason = "raw mutable static proposal columns cannot be safely iterated by reference"
        )]
        unsafe {
            for index in 0..MAX_PENDING_RESIZE_PROPOSALS {
                let candidate = PENDING_RESIZE_REQUEST_IDS[index];
                if candidate > applied_through
                    && candidate <= request_id
                    && next.is_none_or(|(_, next_id)| candidate < next_id)
                {
                    next = Some((index, candidate));
                }
            }
            let Some((index, candidate)) = next else {
                break;
            };
            apply_boundary(
                ptr::addr_of_mut!(DURABLE_LAYOUT).cast::<LayoutTransform>(),
                LAYOUT_GEOMETRY_PANE_COUNT.min(MAX_PANES),
                PENDING_RESIZE_AXES[index],
                PENDING_RESIZE_NEGATIVE_MASKS[index],
                PENDING_RESIZE_POSITIVE_MASKS[index],
                PENDING_RESIZE_COORDINATES[index],
            );
            applied = true;
            applied_through = candidate;
        }
    }
    if applied {
        unsafe {
            LAYOUT_GEOMETRY_CUSTOMIZED = true;
        }
    }
    remove_resize_proposals_through(request_id);
    rebuild_requested_layout();
}

fn reject_resize_proposal(request_id: u64) {
    remove_resize_proposal(request_id);
    rebuild_requested_layout();
}

fn handle_synchronized_layout_notification(message: &WorkspaceMessage) -> bool {
    match message.message_type.as_str() {
        "accepted" => true,
        "completed" | "degraded" => {
            if let Some(epoch) = message.resize_epoch {
                unsafe {
                    LAYOUT_BASE_PRESENTED_REVISION =
                        LAYOUT_BASE_PRESENTED_REVISION.max(epoch.target_layout_revision);
                }
            }
            if let Some(request_id) = message.request_id {
                commit_resize_proposal(request_id);
            }
            true
        }
        "rejected" | "aborted" | "superseded" => {
            if let Some(request_id) = message.request_id {
                reject_resize_proposal(request_id);
            }
            true
        }
        _ => false,
    }
}

fn handle_layout_pointer(output: &mut [u8], message: &WorkspaceMessage) -> i32 {
    let Some(zone) = message.zone else { return 0 };
    let Some(interaction) = message.interaction.filter(|interaction| *interaction != 0) else {
        return 0;
    };
    let Some(event_sequence) = message.event_sequence.filter(|sequence| *sequence != 0) else {
        return 0;
    };
    let Some(phase) = message.phase.as_deref() else {
        return 0;
    };
    let Some(position) = message
        .position
        .filter(|position| position.u.is_finite() && position.v.is_finite())
    else {
        return 0;
    };
    let (panes, pane_backends, focus, fullscreen, lease, _) = layout_actor_state();
    let Some(lease) = lease.filter(|lease| Some(*lease) == message.lease) else {
        return 0;
    };
    if fullscreen || panes.len() < 2 {
        return 0;
    }

    unsafe {
        if let Some(inventory_revision) = message.inventory_revision {
            LAYOUT_REVISION = inventory_revision;
        }
        if let Some(base_layout_revision) = message
            .base_layout_revision
            .or(message.presented_layout_revision)
            .or(message.layout_revision)
        {
            LAYOUT_BASE_PRESENTED_REVISION = base_layout_revision;
        }
    }

    if phase == "button-press" && message.button == Some(PRIMARY_BUTTON) {
        let mut boundaries = [LayoutBoundary::EMPTY; MAX_LAYOUT_BOUNDARIES];
        let boundary_count = collect_layout_boundaries(panes, pane_backends, true, &mut boundaries);
        let Some(boundary_index) = zone
            .checked_sub(DIVIDER_ZONE_ID)
            .map(|index| index as usize)
        else {
            return 0;
        };
        if boundary_index >= boundary_count
            || !boundary_is_waitable(boundaries[boundary_index], panes)
        {
            return 0;
        }
        let boundary = boundaries[boundary_index];
        let pointer_coordinate = if boundary.axis == 0 {
            position.u as f32
        } else {
            position.v as f32
        };
        unsafe {
            ACTIVE_POINTER_INTERACTION = interaction;
            HAS_ACTIVE_POINTER_INTERACTION = true;
            LAST_POINTER_EVENT_SEQUENCE = event_sequence;
            ACTIVE_POINTER_ZONE = zone;
            ACTIVE_BOUNDARY_AXIS = boundary.axis;
            ACTIVE_BOUNDARY_NEGATIVE_MASK = boundary.negative_mask;
            ACTIVE_BOUNDARY_POSITIVE_MASK = boundary.positive_mask;
            POINTER_BOUNDARY_OFFSET = boundary.fixed - pointer_coordinate;
        }
        return 0;
    }

    let active = unsafe {
        HAS_ACTIVE_POINTER_INTERACTION
            && ACTIVE_POINTER_INTERACTION == interaction
            && ACTIVE_POINTER_ZONE == zone
            && event_sequence > LAST_POINTER_EVENT_SEQUENCE
    };
    if !active {
        return 0;
    }
    if phase == "cancel" {
        clear_pointer_interaction();
        return 0;
    }
    if phase != "motion" && !(phase == "button-release" && message.button == Some(PRIMARY_BUTTON)) {
        return 0;
    }

    let boundary = unsafe {
        LayoutBoundary {
            axis: ACTIVE_BOUNDARY_AXIS,
            fixed: 0.0,
            start: 0.0,
            end: 0.0,
            negative_mask: ACTIVE_BOUNDARY_NEGATIVE_MASK,
            positive_mask: ACTIVE_BOUNDARY_POSITIVE_MASK,
        }
    };
    let pointer_coordinate = if boundary.axis == 0 {
        position.u as f32
    } else {
        position.v as f32
    };
    let fixed = clamped_boundary_coordinate(
        boundary.axis,
        boundary.negative_mask,
        boundary.positive_mask,
        pointer_coordinate + unsafe { POINTER_BOUNDARY_OFFSET },
    );
    unsafe {
        apply_boundary(
            ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
            LAYOUT_GEOMETRY_PANE_COUNT.min(MAX_PANES),
            boundary.axis,
            boundary.negative_mask,
            boundary.positive_mask,
            fixed,
        );
        LAST_POINTER_EVENT_SEQUENCE = event_sequence;
    }
    let request_id = next_layout_request_id();
    let len = write_synchronized_layout_outbox(
        output,
        panes,
        pane_backends,
        SynchronizedRequestContext {
            focus,
            fullscreen: false,
            lease,
            interaction: Some(interaction),
            event_sequence: Some(event_sequence),
            request_id,
            native_resize_revision: None,
            use_requested_geometry: true,
        },
    );
    if len > 0 {
        remember_resize_proposal(request_id, boundary, fixed);
    } else {
        rebuild_requested_layout();
    }
    if phase == "button-release" {
        clear_pointer_interaction();
    }
    len
}

fn next_layout_request_id() -> u64 {
    unsafe {
        let request_id = NEXT_LAYOUT_REQUEST_ID.max(1);
        NEXT_LAYOUT_REQUEST_ID = request_id.saturating_add(1).max(1);
        request_id
    }
}

fn clear_pointer_interaction() {
    unsafe {
        HAS_ACTIVE_POINTER_INTERACTION = false;
        ACTIVE_POINTER_INTERACTION = 0;
        LAST_POINTER_EVENT_SEQUENCE = 0;
        ACTIVE_POINTER_ZONE = 0;
        ACTIVE_BOUNDARY_AXIS = 0;
        ACTIVE_BOUNDARY_NEGATIVE_MASK = 0;
        ACTIVE_BOUNDARY_POSITIVE_MASK = 0;
        POINTER_BOUNDARY_OFFSET = 0.0;
    }
}

fn write_synchronized_layout_outbox(
    output: &mut [u8],
    panes: &[u64],
    pane_backends: &[u64],
    request: SynchronizedRequestContext,
) -> i32 {
    let (inventory_revision, base_layout_revision) =
        unsafe { (LAYOUT_REVISION, LAYOUT_BASE_PRESENTED_REVISION) };
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":[{"actor":"host","payload":["#);
    let mut selection = PayloadWriter::new(&mut writer);
    write_set_pointer_policy_payload(&mut selection, request.lease);
    writer.bytes(br#"]},{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    payload.bytes(br#"{"type":"submit-synchronized-layout","request_id":"#);
    payload.u64(request.request_id);
    if let Some(interaction) = request.interaction {
        payload.bytes(br#", "interaction":"#);
        payload.u64(interaction);
    }
    if let Some(event_sequence) = request.event_sequence {
        payload.bytes(br#", "event_sequence":"#);
        payload.u64(event_sequence);
    }
    if let Some(native_resize_revision) = request.native_resize_revision {
        payload.bytes(br#", "native_resize_revision":"#);
        payload.u64(native_resize_revision);
    }
    payload.bytes(br#", "lease":{"id":"#);
    payload.u64(request.lease.id);
    payload.bytes(br#", "generation":"#);
    payload.u64(request.lease.generation);
    payload.bytes(br#"},"inventory_revision":"#);
    payload.u64(inventory_revision);
    payload.bytes(br#", "base_layout_revision":"#);
    payload.u64(base_layout_revision);
    payload.bytes(br#", "fragment":{"layout":"#);
    write_resolved_layout_payload(
        &mut payload,
        panes,
        pane_backends,
        request.focus,
        request.fullscreen,
        request.use_requested_geometry,
    );
    payload.bytes(br#"}"#);
    if let Some(focus) = request.focus {
        payload.bytes(br#", "focus":"#);
        payload.u64(focus);
    }
    payload.bytes(br#", "wait_for_panes":["#);
    let fullscreen_pane = request
        .fullscreen
        .then(|| {
            request
                .focus
                .filter(|focus| panes.contains(focus))
                .or_else(|| panes.first().copied())
        })
        .flatten();
    let mut first = true;
    for pane in panes {
        if !layout_waitable_panes().contains(pane)
            || fullscreen_pane.is_some_and(|visible| visible != *pane)
        {
            continue;
        }
        if !first {
            payload.byte(b',');
        }
        first = false;
        payload.u64(*pane);
    }
    payload.bytes(br#"],"presentation":"scale-then-atomic"}"#);
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact host epoch and current lease fragment form one bounded response"
)]
fn write_layout_region_outbox(
    output: &mut [u8],
    panes: &[u64],
    pane_backends: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    lease: LayoutLeaseToken,
    inventory_revision: u64,
    epoch: LayoutNotificationEpoch,
) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"messages":[{"actor":"host","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    payload.bytes(br#"{"type":"submit-layout-region","resize_epoch":{"id":"#);
    payload.u64(epoch.id);
    payload.bytes(br#", "lease":"#);
    payload.u64(epoch.lease);
    payload.bytes(br#", "lease_generation":"#);
    payload.u64(epoch.lease_generation);
    payload.bytes(br#", "base_layout_revision":"#);
    payload.u64(epoch.base_layout_revision);
    payload.bytes(br#", "target_layout_revision":"#);
    payload.u64(epoch.target_layout_revision);
    payload.bytes(br#"},"lease":{"id":"#);
    payload.u64(lease.id);
    payload.bytes(br#", "generation":"#);
    payload.u64(lease.generation);
    payload.bytes(br#"},"inventory_revision":"#);
    payload.u64(inventory_revision);
    payload.bytes(br#", "fragment":{"layout":"#);
    write_resolved_layout_payload(&mut payload, panes, pane_backends, focus, fullscreen, false);
    payload.bytes(br#"}"#);
    if let Some(focus) = focus {
        payload.bytes(br#", "focus":"#);
        payload.u64(focus);
    }
    payload.byte(b'}');
    writer.bytes(br#"]}]}"#);
    writer.finish()
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
    pane_backends: &[u64],
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
    payload.bytes(br#","layout":"#);
    write_resolved_layout_payload(payload, panes, pane_backends, focus, fullscreen, false);
    payload.bytes(br#", "focus":"#);
    if let Some(focus) = focus {
        payload.u64(focus);
    } else {
        payload.bytes(b"null");
    }
    payload.byte(b'}');
}

fn write_set_pointer_policy_payload(payload: &mut PayloadWriter<'_, '_>, lease: LayoutLeaseToken) {
    payload.bytes(br#"{"type":"set-pointer-policy","lease":{"id":"#);
    payload.u64(lease.id);
    payload.bytes(br#", "generation":"#);
    payload.u64(lease.generation);
    payload.bytes(br#"},"provider":"workspace"}"#);
}

fn write_resolved_layout_payload(
    payload: &mut PayloadWriter<'_, '_>,
    panes: &[u64],
    pane_backends: &[u64],
    focus: Option<u64>,
    fullscreen: bool,
    use_requested_geometry: bool,
) {
    payload.bytes(br#"{"items":["#);
    let pane_count = panes.len();
    if fullscreen && pane_count > 0 {
        let pane = focus
            .filter(|focus| panes.contains(focus))
            .unwrap_or(panes[0]);
        write_pane_item_payload(payload, pane, 0.0, -1.0, -1.0, 2.0, 2.0);
    } else {
        let mut index = 0;
        while index < pane_count {
            if index > 0 {
                payload.byte(b',');
            }
            let placement = layout_transform(index, panes, pane_backends, use_requested_geometry);
            write_pane_item_payload(
                payload,
                panes[index],
                index as f32,
                placement.offset_u,
                placement.offset_v,
                placement.size_u,
                placement.size_v,
            );
            index += 1;
        }
    }
    payload.byte(b']');
    if !fullscreen && pane_count > 1 {
        let mut boundaries = [LayoutBoundary::EMPTY; MAX_LAYOUT_BOUNDARIES];
        let boundary_count = collect_layout_boundaries(
            panes,
            pane_backends,
            use_requested_geometry,
            &mut boundaries,
        );
        payload.bytes(br#", "zones":["#);
        let mut first = true;
        for (index, boundary) in boundaries[..boundary_count].iter().copied().enumerate() {
            if !boundary_is_waitable(boundary, panes) {
                continue;
            }
            if !first {
                payload.byte(b',');
            }
            first = false;
            write_interaction_zone_payload(payload, index, boundary);
        }
        payload.bytes(br#"],"dividers":["#);
        first = true;
        for (index, boundary) in boundaries[..boundary_count].iter().copied().enumerate() {
            if !boundary_is_waitable(boundary, panes) {
                continue;
            }
            if !first {
                payload.byte(b',');
            }
            first = false;
            write_divider_payload(payload, index, boundary);
        }
        payload.byte(b']');
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

fn boundary_transform(boundary: LayoutBoundary, hit_zone: bool) -> LayoutTransform {
    let (width, height) = unsafe {
        (
            LAYOUT_VIEWPORT_WIDTH.max(1) as f32,
            LAYOUT_VIEWPORT_HEIGHT.max(1) as f32,
        )
    };
    if boundary.axis == 0 {
        let size = if hit_zone { 16.0 / width } else { 2.0 / width };
        LayoutTransform {
            offset_u: boundary.fixed - size * 0.5,
            offset_v: boundary.start,
            size_u: size,
            size_v: boundary.end - boundary.start,
        }
    } else {
        let size = if hit_zone {
            16.0 / height
        } else {
            2.0 / height
        };
        LayoutTransform {
            offset_u: boundary.start,
            offset_v: boundary.fixed - size * 0.5,
            size_u: boundary.end - boundary.start,
            size_v: size,
        }
    }
}

fn write_interaction_zone_payload(
    payload: &mut PayloadWriter<'_, '_>,
    index: usize,
    boundary: LayoutBoundary,
) {
    let transform = boundary_transform(boundary, true);
    let mut item = [0_u8; 512];
    let mut writer = Writer::new(&mut item);
    writer.bytes(br#"{"id":"#);
    writer.u64(divider_zone_id(index));
    writer.bytes(br#", "transform":{"offset":{"u":"#);
    writer.f32(transform.offset_u);
    writer.bytes(br#", "v":"#);
    writer.f32(transform.offset_v);
    writer.bytes(br#"},"size":{"u":"#);
    writer.f32(transform.size_u);
    writer.bytes(br#", "v":"#);
    writer.f32(transform.size_v);
    writer.bytes(br#"}},"z":1000,"event_mask":"#);
    writer.u64(u64::from(DIVIDER_EVENT_MASK));
    if boundary.axis == 0 {
        writer.bytes(br#", "cursor":"col-resize","input":"drag"}"#);
    } else {
        writer.bytes(br#", "cursor":"row-resize","input":"drag"}"#);
    }
    let len = writer.finish();
    if len > 0 {
        payload.bytes(&item[..len as usize]);
    }
}

fn write_divider_payload(
    payload: &mut PayloadWriter<'_, '_>,
    index: usize,
    boundary: LayoutBoundary,
) {
    let transform = boundary_transform(boundary, false);
    let mut item = [0_u8; 512];
    let mut writer = Writer::new(&mut item);
    writer.bytes(br#"{"id":"#);
    writer.u64(divider_item_id(index));
    writer.bytes(br#", "zone":"#);
    writer.u64(divider_zone_id(index));
    // Native chrome is a separate paint phase. Keep this divider below the
    // ordinary pane items so the real proof detects any z-only paint-order
    // regression.
    writer.bytes(br#", "z":-100,"transform":{"offset":{"u":"#);
    writer.f32(transform.offset_u);
    writer.bytes(br#", "v":"#);
    writer.f32(transform.offset_v);
    writer.bytes(br#"},"size":{"u":"#);
    writer.f32(transform.size_u);
    writer.bytes(br#", "v":"#);
    writer.f32(transform.size_v);
    if boundary.axis == 0 {
        writer.bytes(br#"}},"axis":"vertical","role":"split"}"#);
    } else {
        writer.bytes(br#"}},"axis":"horizontal","role":"split"}"#);
    }
    let len = writer.finish();
    if len > 0 {
        payload.bytes(&item[..len as usize]);
    }
}

fn write_consume(output: &mut [u8], messages: &[Message<'_>]) -> i32 {
    finish_response(write_consume_payload(output, messages))
}

fn write_pointer_policy_result(output: &mut [u8], capture: &str, actor: &str) -> i32 {
    const PROVIDER_PAYLOAD: &[u8] = br#"{"kind":"divider"}"#;

    let mut writer = Writer::new(output);
    writer.bytes(br#"{"delivery":"consume","capture":"#);
    writer.byte(b'"');
    writer.bytes(capture.as_bytes());
    writer.bytes(br#"","messages":[{"actor":""#);
    writer.bytes(actor.as_bytes());
    writer.bytes(br#"","payload":["#);
    let mut index = 0;
    while index < PROVIDER_PAYLOAD.len() {
        if index > 0 {
            writer.byte(b',');
        }
        writer.u64(u64::from(PROVIDER_PAYLOAD[index]));
        index += 1;
    }
    writer.bytes(br#"]}]}"#);
    writer.finish()
}

fn write_keybindings_show(output: &mut [u8], bindings: &[BindingOverride]) -> i32 {
    let mut writer = Writer::new(output);
    writer.bytes(br#"{"decision":"consume","messages":[{"actor":"keybindings","payload":["#);
    let mut payload = PayloadWriter::new(&mut writer);
    payload.bytes(br#"{"type":"keybindings-show","bindings":["#);
    let mut first = true;
    for binding in bindings {
        if !first {
            payload.byte(b',');
        }
        first = false;
        payload.bytes(br#"{"action":""#);
        payload.quoted_body(binding.action.as_bytes());
        payload.bytes(br#"","key_code":"#);
        payload.u64(binding.key_code.max(0) as u64);
        payload.bytes(br#","modifiers":"#);
        payload.u64(binding.modifiers.max(0) as u64);
        payload.byte(b'}');
    }
    payload.bytes(br#"]}"#);
    writer.bytes(br#"]}]}"#);
    finish_response(writer.finish())
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
            Message::InboxAction(_) => "inbox",
            Message::CodingAgentAction(_)
            | Message::DiffViewerAction(_)
            | Message::TerminalAction(_)
            | Message::EditorAction(_) => "brood.client-init",
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
            Message::CodingAgentAction(action) => {
                payload.bytes(br#"{"kind":"coding-agent-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
            Message::DiffViewerAction(action) => {
                payload.bytes(br#"{"kind":"diff-viewer-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
            Message::InboxAction(action) => {
                payload.bytes(br#"{"kind":"inbox-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
            Message::TerminalAction(action) => {
                payload.bytes(br#"{"kind":"terminal-action","action":"#);
                payload.byte(b'"');
                payload.quoted_body(action.as_bytes());
                payload.bytes(br#""}"#);
            }
            Message::EditorAction(action) => {
                payload.bytes(br#"{"kind":"editor-action","action":"#);
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

fn focused_raw_control_plugin(state: &PolicyState) -> bool {
    let Some(focused) = state.focused_pane else {
        return false;
    };
    state.app_panes.iter().any(|item| {
        item.pane == focused
            && (item.app_id.as_bytes().starts_with(b"terminal-ui")
                || item.app_id.as_bytes().starts_with(b"neovim-ui"))
    })
}

fn focused_keybinding_editor(state: &PolicyState) -> bool {
    let Some(focused) = state.focused_pane else {
        return false;
    };
    state
        .app_panes
        .iter()
        .any(|item| item.pane == focused && item.app_id.as_str() == "keybindings")
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
    use std::{format, string::ToString, vec::Vec};

    use super::*;
    use serde_json::{json, Value};

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn host_payloads(output: &[u8], len: i32) -> Vec<Value> {
        assert!(len > 0);
        let envelope: Value = serde_json::from_slice(&output[..len as usize]).unwrap();
        envelope["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| {
                let encoded = message["payload"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>();
                serde_json::from_slice(&encoded).unwrap()
            })
            .collect()
    }

    fn host_payload(output: &[u8], len: i32) -> Value {
        host_payloads(output, len).pop().unwrap()
    }

    fn pointer_message(
        phase: &str,
        position: f64,
        sequence: u64,
        button: Option<u32>,
    ) -> WorkspaceMessage {
        pointer_message_for(DIVIDER_ZONE_ID, phase, position, 0.0, sequence, button)
    }

    fn pointer_message_for(
        zone: u64,
        phase: &str,
        u: f64,
        v: f64,
        sequence: u64,
        button: Option<u32>,
    ) -> WorkspaceMessage {
        let button = button.map_or_else(|| "null".into(), |button| button.to_string());
        let json = format!(
            "{{\"type\":\"layout-pointer\",\"interaction\":99,\"event_sequence\":{sequence},\"lease\":{{\"id\":5,\"generation\":2}},\"zone\":{zone},\"phase\":\"{phase}\",\"position\":{{\"u\":{u},\"v\":{v}}},\"button\":{button},\"modifiers\":0,\"inventory_revision\":7,\"base_layout_revision\":3}}"
        );
        decode_json(json.as_bytes()).unwrap()
    }

    fn synchronized_notification(kind: &str, request_id: u64) -> WorkspaceMessage {
        decode_json(format!(r#"{{"type":"{kind}","request_id":{request_id}}}"#).as_bytes()).unwrap()
    }

    fn completed_notification(request_id: u64, target_layout_revision: u64) -> WorkspaceMessage {
        decode_json(
            format!(
                r#"{{"type":"completed","request_id":{request_id},"resize_epoch":{{"target_layout_revision":{target_layout_revision}}}}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn divider_policy_and_actor_preserve_one_synchronized_drag() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let layout_state: WorkspaceMessage = decode_json(
            br#"{"type":"layout-state","revision":7,"inventory_revision":7,"layout_revision":3,"presented_layout_revision":3,"lease":{"id":5,"generation":2},"waitable_panes":[11,22],"visible_panes":[11,22],"size":{"w":1280,"h":720}}"#,
        )
        .unwrap();
        assert_eq!(layout_state.inventory_revision, Some(7));
        assert_eq!(layout_state.presented_layout_revision, Some(3));
        assert_eq!(layout_state.layout_revision, Some(3));
        assert_eq!(layout_state.waitable_panes.as_slice(), &[11, 22]);

        let press: PointerPolicyQuery = decode_json(
            format!(
                "{{\"zone\":{DIVIDER_ZONE_ID},\"phase\":\"button-press\",\"button\":{PRIMARY_BUTTON},\"captured\":false}}"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(pointer_capture_decision(&press), Some("acquire"));
        let motion: PointerPolicyQuery = decode_json(
            format!(
                "{{\"zone\":{DIVIDER_ZONE_ID},\"phase\":\"motion\",\"button\":null,\"captured\":true}}"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(pointer_capture_decision(&motion), Some("unchanged"));
        let mut policy_output = [0_u8; 512];
        let policy_len =
            write_pointer_policy_result(&mut policy_output, "unchanged", "lease-holder");
        let policy: Value = serde_json::from_slice(&policy_output[..policy_len as usize]).unwrap();
        assert_eq!(policy["delivery"], "consume");
        assert_eq!(policy["capture"], "unchanged");
        assert_eq!(policy["messages"][0]["actor"], "lease-holder");
        let arbitrary_zone: PointerPolicyQuery = decode_json(
            format!(
                "{{\"zone\":7,\"phase\":\"button-press\",\"button\":{PRIMARY_BUTTON},\"captured\":false}}"
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(pointer_capture_decision(&arbitrary_zone), Some("acquire"));
        let policy_len = write_pointer_policy_result(&mut policy_output, "acquire", "lease-holder");
        let policy: Value = serde_json::from_slice(&policy_output[..policy_len as usize]).unwrap();
        assert_eq!(policy["messages"][0]["actor"], "lease-holder");
        let other_zone: PointerPolicyQuery =
            decode_json(br#"{"zone":7,"phase":"motion","button":null,"captured":true}"#).unwrap();
        assert_eq!(pointer_capture_decision(&other_zone), Some("unchanged"));

        clear_pointer_interaction();
        clear_resize_proposals();
        unsafe {
            NEXT_LAYOUT_REQUEST_ID = 1;
        }
        let panes = [11, 22];
        let realms = [
            PaneRealm {
                pane: 11,
                backend: 1,
            },
            PaneRealm {
                pane: 22,
                backend: 1,
            },
        ];
        store_layout_actor_state(
            &panes,
            &realms,
            Some(11),
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
            Some(3),
            &panes,
            LayoutStateSize { w: 1280, h: 720 },
        );
        let narrow_left = initial_layout_transform(0, &panes, &[1, 1], 640, 686);
        let narrow_right = initial_layout_transform(1, &panes, &[1, 1], 640, 686);
        assert_eq!((narrow_left.offset_u, narrow_left.size_u), (-1.0, 1.0));
        assert_eq!((narrow_right.offset_u, narrow_right.size_u), (0.0, 1.0));

        let mut output = [0_u8; OUTBOX_BYTES];
        assert_eq!(
            handle_layout_pointer(
                &mut output,
                &pointer_message("button-press", 0.0, 1, Some(PRIMARY_BUTTON)),
            ),
            0
        );
        let motion_len =
            handle_layout_pointer(&mut output, &pointer_message("motion", 0.4, 2, None));
        let motion = host_payload(&output, motion_len);
        assert_eq!(motion["type"], "submit-synchronized-layout");
        assert_eq!(motion["request_id"], 1);
        assert_eq!(motion["interaction"], 99);
        assert_eq!(motion["event_sequence"], 2);
        assert_eq!(motion["inventory_revision"], 7);
        assert_eq!(motion["base_layout_revision"], 3);
        assert_eq!(motion["focus"], 11);
        assert!(motion.get("native_resize_revision").is_none());
        assert_eq!(motion["wait_for_panes"], json!([11, 22]));
        assert_eq!(motion["presentation"], "scale-then-atomic");
        assert_eq!(
            motion["fragment"]["layout"]["zones"][0]["id"],
            DIVIDER_ZONE_ID
        );
        assert_eq!(
            motion["fragment"]["layout"]["dividers"][0]["zone"],
            DIVIDER_ZONE_ID
        );
        assert!(
            motion["fragment"]["layout"]["dividers"][0]["z"]
                .as_f64()
                .is_some_and(|divider_z| {
                    motion["fragment"]["layout"]["items"]
                        .as_array()
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item["z"].as_f64().is_some_and(|pane_z| pane_z > divider_z)
                            })
                        })
                }),
            "the real actor fixture must keep its divider below ordinary pane content"
        );
        let zone_width = motion["fragment"]["layout"]["zones"][0]["transform"]["size"]["u"]
            .as_f64()
            .unwrap();
        let zone_physical_width = zone_width * 1280.0 * 0.5;
        assert!((7.5..=9.0).contains(&zone_physical_width));
        let motion_primary_width = motion["fragment"]["layout"]["items"][0]["transform"]["size"]
            ["u"]
            .as_f64()
            .unwrap();
        assert!((motion_primary_width - 1.4).abs() < 0.01);

        let release_len = handle_layout_pointer(
            &mut output,
            &pointer_message("button-release", 0.5, 3, Some(PRIMARY_BUTTON)),
        );
        let release = host_payload(&output, release_len);
        assert_eq!(release["request_id"], 2);
        assert_eq!(release["event_sequence"], 3);
        assert_eq!(
            release["fragment"]["layout"]["items"][0]["transform"]["size"]["u"],
            1.5
        );
        assert!(!unsafe { HAS_ACTIVE_POINTER_INTERACTION });

        // Ordinary actor updates cannot publish the proposed geometry before
        // the host confirms that the synchronized request was presented.
        let len = write_layout_actor_update_outbox(
            &mut output,
            &panes,
            &[1, 1],
            Some(11),
            false,
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
        );
        let before_completion = host_payload(&output, len);
        assert_eq!(
            before_completion["layout"]["items"][0]["transform"]["size"]["u"],
            1.0
        );

        assert!(handle_synchronized_layout_notification(
            &synchronized_notification("superseded", 1)
        ));
        assert!(handle_synchronized_layout_notification(
            &completed_notification(2, 8)
        ));
        assert_eq!(unsafe { LAYOUT_BASE_PRESENTED_REVISION }, 8);
        let len = write_layout_actor_update_outbox(
            &mut output,
            &panes,
            &[1, 1],
            Some(11),
            false,
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
        );
        let durable = host_payload(&output, len);
        assert_eq!(durable["layout"]["items"][0]["transform"]["size"]["u"], 1.5);
        assert_eq!(durable["layout"]["zones"][0]["event_mask"], 15);

        store_layout_actor_state(
            &panes,
            &realms,
            Some(11),
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
            Some(8),
            &panes,
            LayoutStateSize { w: 1000, h: 720 },
        );
        assert!((layout_transform(0, &panes, &[1, 1], false).size_u - 1.5).abs() < f32::EPSILON);

        let boundary = LayoutBoundary {
            axis: 0,
            fixed: 0.5,
            start: -1.0,
            end: 1.0,
            negative_mask: 1,
            positive_mask: 2,
        };
        apply_boundary(
            ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
            2,
            boundary.axis,
            boundary.negative_mask,
            boundary.positive_mask,
            0.8,
        );
        remember_resize_proposal(3, boundary, 0.8);
        assert!(handle_synchronized_layout_notification(
            &synchronized_notification("rejected", 3)
        ));
        assert!((layout_transform(0, &panes, &[1, 1], true).size_u - 1.5).abs() < f32::EPSILON);
        assert!((layout_transform(0, &panes, &[1, 1], false).size_u - 1.5).abs() < f32::EPSILON);

        // A completion for an older request advances durable state without
        // snapping an already-proposed newer pointer sample backwards.
        reset_layout_geometry(&panes, &[1, 1], 1000, 720);
        apply_boundary(
            ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
            2,
            0,
            1,
            2,
            0.2,
        );
        remember_resize_proposal(10, boundary, 0.2);
        apply_boundary(
            ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
            2,
            0,
            1,
            2,
            0.4,
        );
        remember_resize_proposal(11, boundary, 0.4);
        assert!(handle_synchronized_layout_notification(
            &completed_notification(10, 9)
        ));
        assert_eq!(unsafe { LAYOUT_BASE_PRESENTED_REVISION }, 9);
        assert!((layout_transform(0, &panes, &[1, 1], false).size_u - 1.2).abs() < f32::EPSILON);
        assert!((layout_transform(0, &panes, &[1, 1], true).size_u - 1.4).abs() < f32::EPSILON);
        assert!(handle_synchronized_layout_notification(
            &synchronized_notification("rejected", 11)
        ));
        unsafe {
            assert!((REQUESTED_LAYOUT[0].size_u - 1.2).abs() < f32::EPSILON);
            apply_boundary(
                ptr::addr_of_mut!(REQUESTED_LAYOUT).cast::<LayoutTransform>(),
                2,
                0,
                1,
                2,
                0.6,
            );
        }
        remember_resize_proposal(12, boundary, 0.6);
        assert!(handle_synchronized_layout_notification(
            &synchronized_notification("aborted", 12)
        ));
        assert!((layout_transform(0, &panes, &[1, 1], true).size_u - 1.2).abs() < f32::EPSILON);
        assert_native_window_resize_request();
    }

    #[test]
    fn every_shared_boundary_in_a_three_pane_grid_is_draggable() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let panes = [11, 22, 33];
        let realms = panes.map(|pane| PaneRealm { pane, backend: 1 });
        store_layout_actor_state(
            &panes,
            &realms,
            Some(11),
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
            Some(3),
            &panes,
            LayoutStateSize { w: 1600, h: 900 },
        );
        unsafe {
            NEXT_LAYOUT_REQUEST_ID = 100;
        }

        let mut output = [0_u8; OUTBOX_BYTES];
        let initial_len = write_layout_actor_update_outbox(
            &mut output,
            &panes,
            &[1, 1, 1],
            Some(11),
            false,
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
        );
        let initial = host_payload(&output, initial_len);
        assert_eq!(initial["layout"]["zones"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            initial["layout"]["dividers"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(initial["layout"]["dividers"][0]["axis"], "vertical");
        assert_eq!(initial["layout"]["dividers"][1]["axis"], "horizontal");

        assert_eq!(
            handle_layout_pointer(
                &mut output,
                &pointer_message_for(
                    divider_zone_id(0),
                    "button-press",
                    0.0,
                    -0.5,
                    1,
                    Some(PRIMARY_BUTTON),
                ),
            ),
            0
        );
        let vertical_len = handle_layout_pointer(
            &mut output,
            &pointer_message_for(divider_zone_id(0), "motion", 0.25, -0.5, 2, None),
        );
        let vertical = host_payload(&output, vertical_len);
        let items = vertical["fragment"]["layout"]["items"].as_array().unwrap();
        assert_eq!(items[0]["transform"]["size"]["u"], 1.25);
        assert_eq!(items[1]["transform"]["offset"]["u"], 0.25);
        assert_eq!(items[2]["transform"]["size"]["u"], 2.0);
        let release_len = handle_layout_pointer(
            &mut output,
            &pointer_message_for(
                divider_zone_id(0),
                "button-release",
                0.25,
                -0.5,
                3,
                Some(PRIMARY_BUTTON),
            ),
        );
        assert!(release_len > 0);
        assert!(handle_synchronized_layout_notification(
            &completed_notification(101, 8)
        ));

        assert_eq!(
            handle_layout_pointer(
                &mut output,
                &pointer_message_for(
                    divider_zone_id(1),
                    "button-press",
                    0.0,
                    0.0,
                    1,
                    Some(PRIMARY_BUTTON),
                ),
            ),
            0
        );
        let horizontal_len = handle_layout_pointer(
            &mut output,
            &pointer_message_for(divider_zone_id(1), "motion", 0.0, 0.3, 2, None),
        );
        let horizontal = host_payload(&output, horizontal_len);
        let items = horizontal["fragment"]["layout"]["items"]
            .as_array()
            .unwrap();
        let value = |item: usize, field: &str, axis: &str| {
            items[item]["transform"][field][axis].as_f64().unwrap()
        };
        assert!((value(0, "size", "v") - 1.3).abs() < 0.002);
        assert!((value(1, "size", "v") - 1.3).abs() < 0.002);
        assert!((value(2, "offset", "v") - 0.3).abs() < 0.002);
        assert!((value(2, "size", "v") - 0.7).abs() < 0.002);
        assert_eq!(
            horizontal["fragment"]["layout"]["zones"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn maximum_inventory_keeps_all_panes_and_multiple_drag_zones() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let panes = core::array::from_fn::<_, MAX_PANES, _>(|index| index as u64 + 1);
        let realms = panes.map(|pane| PaneRealm { pane, backend: 1 });
        store_layout_actor_state(
            &panes,
            &realms,
            Some(1),
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
            Some(3),
            &panes,
            LayoutStateSize { w: 1920, h: 1080 },
        );
        let mut output = [0_u8; OUTBOX_BYTES];
        let len = write_layout_actor_update_outbox(
            &mut output,
            &panes,
            &[1; MAX_PANES],
            Some(1),
            false,
            Some(LayoutLeaseToken {
                id: 5,
                generation: 2,
            }),
            Some(7),
        );
        let publication = host_payload(&output, len);
        assert_eq!(
            publication["layout"]["items"].as_array().map(Vec::len),
            Some(MAX_PANES)
        );
        assert!(publication["layout"]["zones"]
            .as_array()
            .is_some_and(|zones| zones.len() > 1));
    }

    fn assert_native_window_resize_request() {
        let message: WorkspaceMessage = decode_json(
            br#"{"type":"layout-state","revision":7,"presented_layout_revision":3,"native_resize_revision":41,"lease":{"id":5,"generation":2},"waitable_panes":[11,22],"visible_panes":[11,22],"focused_pane":22,"size":{"w":800,"h":720}}"#,
        )
        .unwrap();
        assert_eq!(message.native_resize_revision, Some(41));
        let panes = [11, 22];
        let realms = [
            PaneRealm {
                pane: 11,
                backend: 1,
            },
            PaneRealm {
                pane: 22,
                backend: 1,
            },
        ];
        store_layout_actor_state(
            &panes,
            &realms,
            message.focused_pane,
            message.lease,
            message.revision,
            message.presented_layout_revision,
            &message.waitable_panes,
            message.size,
        );
        let mut output = [0_u8; OUTBOX_BYTES];
        let len = write_synchronized_layout_outbox(
            &mut output,
            &panes,
            &[1, 1],
            SynchronizedRequestContext {
                focus: Some(22),
                fullscreen: true,
                lease: LayoutLeaseToken {
                    id: 5,
                    generation: 2,
                },
                interaction: None,
                event_sequence: None,
                request_id: 9,
                native_resize_revision: Some(41),
                use_requested_geometry: false,
            },
        );
        let messages = host_payloads(&output, len);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["type"], "set-pointer-policy");
        assert_eq!(messages[0]["lease"], json!({"id": 5, "generation": 2}));
        assert_eq!(messages[0]["provider"], "workspace");
        let request = &messages[1];
        assert_eq!(request["type"], "submit-synchronized-layout");
        assert_eq!(request["native_resize_revision"], 41);
        assert!(request.get("event_sequence").is_none());
        assert_eq!(request["focus"], 22);
        assert!(request.get("interaction").is_none());
        assert_eq!(request["wait_for_panes"], json!([22]));
        assert_eq!(
            request["fragment"]["layout"]["items"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            request["fragment"]["layout"]["items"][0]["kind"]["pane"],
            22
        );
    }

    #[test]
    fn delegated_region_reply_echoes_the_exact_host_epoch_and_lease() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let decoded: WorkspaceMessage = decode_json(
            br#"{"type":"layout-region-resized","resize_epoch":{"id":31,"lease":1,"lease_generation":4,"base_layout_revision":7,"target_layout_revision":8},"lease":{"id":5,"generation":2},"size":{"w":900,"h":600},"scale":1.5}"#,
        )
        .unwrap();
        assert_eq!(decoded.message_type.as_str(), "layout-region-resized");
        assert_eq!(decoded.resize_epoch.map(|epoch| epoch.id), Some(31));
        assert_eq!(decoded.lease.map(|lease| lease.generation), Some(2));
        assert_eq!((decoded.size.w, decoded.size.h), (900, 600));
        let mut output = [0_u8; OUTBOX_BYTES];
        let epoch = LayoutNotificationEpoch {
            id: 31,
            lease: 1,
            lease_generation: 4,
            base_layout_revision: 7,
            target_layout_revision: 8,
        };
        let len = write_layout_region_outbox(
            &mut output,
            &[11, 22],
            &[1, 1],
            Some(11),
            false,
            LayoutLeaseToken {
                id: 5,
                generation: 2,
            },
            9,
            epoch,
        );
        let response = host_payload(&output, len);
        assert_eq!(response["type"], "submit-layout-region");
        assert_eq!(response["resize_epoch"]["id"], 31);
        assert_eq!(response["resize_epoch"]["lease"], 1);
        assert_eq!(response["resize_epoch"]["lease_generation"], 4);
        assert_eq!(response["lease"], json!({"id": 5, "generation": 2}));
        assert_eq!(response["inventory_revision"], 9);
        assert_eq!(response["focus"], 11);
        assert_eq!(
            response["fragment"]["layout"]["items"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }
}

#[panic_handler]
#[cfg(not(test))]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
