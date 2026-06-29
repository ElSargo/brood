use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};
use tracing::warn;

use crate::{
    layout::{LayoutAction, LayoutKeymap},
    state::{InputMode, Manifold, PresentationStatus},
};

const KEY_LEFTCTRL: Keycode = Keycode::new(37);
const KEY_RIGHTCTRL: Keycode = Keycode::new(105);
const KEY_LEFTSHIFT: Keycode = Keycode::new(42);
const KEY_RIGHTSHIFT: Keycode = Keycode::new(54);
const KEY_LEFTALT: Keycode = Keycode::new(64);
const KEY_RIGHTALT: Keycode = Keycode::new(108);
const KEY_ESC: Keycode = Keycode::new(1);
const KEY_B: Keycode = Keycode::new(56);
const KEY_COMMA: Keycode = Keycode::new(59);
const KEY_DOT: Keycode = Keycode::new(60);
const KEY_H: Keycode = Keycode::new(43);
const KEY_J: Keycode = Keycode::new(44);
const KEY_K: Keycode = Keycode::new(45);
const KEY_L: Keycode = Keycode::new(46);
const KEY_P: Keycode = Keycode::new(33);
const KEY_S: Keycode = Keycode::new(39);
const KEY_U: Keycode = Keycode::new(30);
const KEY_W: Keycode = Keycode::new(25);

impl Manifold {
    #[allow(dead_code)]
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                self.forward_keyboard_key(
                    event.key_code(),
                    event.state(),
                    Event::time_msec(&event),
                );
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.forward_pointer_motion(pos, event.time_msec());
            }
            InputEvent::PointerButton { event, .. } => {
                self.forward_pointer_button(event.button_code(), event.state(), event.time_msec());
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                });
                let horizontal_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                self.forward_pointer_axis(frame);
            }
            _ => {}
        }
    }

    pub fn forward_keyboard_key(&mut self, key_code: Keycode, state: KeyState, time: u32) {
        let is_ctrl = matches!(key_code, KEY_LEFTCTRL | KEY_RIGHTCTRL);
        let is_shift = matches!(key_code, KEY_LEFTSHIFT | KEY_RIGHTSHIFT);
        let is_alt = matches!(key_code, KEY_LEFTALT | KEY_RIGHTALT);

        if key_code == KEY_B && state == KeyState::Released && self.suppress_mode_toggle_key_release
        {
            self.suppress_mode_toggle_key_release = false;
            self.record_consumed_keyboard_input();
            return;
        }

        if self.suppress_shortcut_prefix_key_release == Some(key_code)
            && state == KeyState::Released
        {
            self.suppress_shortcut_prefix_key_release = None;
            self.record_consumed_keyboard_input();
            return;
        }

        if is_ctrl {
            match state {
                KeyState::Pressed => {
                    self.ctrl_pressed = true;
                    self.ctrl_pressed_key = Some(key_code);
                    self.record_consumed_keyboard_input();
                }
                KeyState::Released if self.ctrl_forwarded_to_client => {
                    self.ctrl_pressed = false;
                    self.ctrl_pressed_key = None;
                    self.ctrl_forwarded_to_client = false;
                    self.forward_keyboard_key_to_focused_client(key_code, state, time);
                }
                KeyState::Released => {
                    self.ctrl_pressed = false;
                    self.ctrl_pressed_key = None;
                    self.record_consumed_keyboard_input();
                }
            }
            return;
        }

        if is_alt {
            match state {
                KeyState::Pressed => {
                    self.alt_pressed = true;
                    self.alt_pressed_key = Some(key_code);
                    self.record_consumed_keyboard_input();
                }
                KeyState::Released if self.alt_forwarded_to_client => {
                    self.alt_pressed = false;
                    self.alt_pressed_key = None;
                    self.alt_forwarded_to_client = false;
                    self.forward_keyboard_key_to_focused_client(key_code, state, time);
                }
                KeyState::Released => {
                    self.alt_pressed = false;
                    self.alt_pressed_key = None;
                    self.record_consumed_keyboard_input();
                }
            }
            return;
        }

        if is_shift {
            match state {
                KeyState::Pressed => {
                    self.shift_pressed = true;
                    self.shift_pressed_key = Some(key_code);
                    self.record_consumed_keyboard_input();
                }
                KeyState::Released if self.shift_forwarded_to_client => {
                    self.shift_pressed = false;
                    self.shift_pressed_key = None;
                    self.shift_forwarded_to_client = false;
                    self.forward_keyboard_key_to_focused_client(key_code, state, time);
                }
                KeyState::Released => {
                    self.shift_pressed = false;
                    self.shift_pressed_key = None;
                    self.record_consumed_keyboard_input();
                }
            }
            return;
        }

        if self.ctrl_pressed && self.shift_pressed && key_code == KEY_ESC {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.shortcut_prefix_active = false;
                self.suppress_shortcut_prefix_key_release = Some(key_code);
                self.toggle_shortcut_passthrough();
            }
            return;
        }

        if self.ctrl_pressed && key_code == KEY_B && !self.shortcut_passthrough() {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.suppress_mode_toggle_key_release = true;
                self.shortcut_prefix_active = true;
                self.toggle_input_mode();
            }
            return;
        }

        if self.shortcut_prefix_active {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.shortcut_prefix_active = false;
                self.suppress_shortcut_prefix_key_release = Some(key_code);
                if key_code == KEY_P {
                    self.set_shortcut_passthrough(true);
                }
            }
            return;
        }

        if self.shortcut_passthrough() {
            self.forward_pressed_modifiers_to_focused_client(time);
            self.forward_keyboard_key_to_focused_client(key_code, state, time);
            return;
        }

        if self.ctrl_pressed
            && let Some(action) = built_in_ctrl_layout_action(key_code)
        {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.apply_layout_action(action);
            }
            return;
        }

        if self.input_mode != InputMode::Normal
            && (self.ctrl_pressed || self.alt_pressed)
            && self.try_wasm_key_handler(key_code, state) == Some(true)
        {
            self.record_consumed_keyboard_input();
            return;
        }

        if self.input_mode == InputMode::Normal {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.handle_normal_mode_key(key_code);
            }
            return;
        }

        if self.input_mode != InputMode::Normal {
            self.forward_pressed_modifiers_to_focused_client(time);
        }
        self.forward_keyboard_key_to_focused_client(key_code, state, time);
    }

    fn handle_normal_mode_key(&mut self, key_code: Keycode) {
        if self
            .try_wasm_key_handler(key_code, KeyState::Pressed)
            .is_some()
        {
            return;
        }

        match key_code {
            KEY_W => {
                if let Err(err) = self.spawn_wezterm_pane() {
                    warn!(?err, "failed to spawn wezterm pane from normal mode");
                }
            }
            _ => {
                if let Some(action) =
                    LayoutKeymap::default().action_for_key(key_code, self.ctrl_pressed)
                {
                    self.apply_layout_action(action);
                }
            }
        }
    }

    fn try_wasm_key_handler(&mut self, key_code: Keycode, state: KeyState) -> Option<bool> {
        let focused_pane = self.focused_pane();
        let panes = self.active_session_wasm_key_panes();
        let result = self.wasm_key_handler.as_mut().map(|handler| {
            handler.handle_key(
                key_code,
                state,
                self.input_mode,
                self.ctrl_pressed,
                self.alt_pressed,
                focused_pane,
                panes,
            )
        })?;
        match result {
            Ok(outcome) => Some(outcome.apply(self)),
            Err(err) => {
                warn!(
                    ?err,
                    "WASM key handler failed; using built-in fallback when available"
                );
                None
            }
        }
    }

    fn record_consumed_keyboard_input(&mut self) {
        let can_sample_visible_latency =
            self.presentation == PresentationStatus::Attached && self.active_window().is_some();
        self.stats.record_input(can_sample_visible_latency);
        self.stats.keyboard_events += 1;
    }

    fn forward_pressed_modifiers_to_focused_client(&mut self, time: u32) {
        if self.ctrl_pressed && !self.ctrl_forwarded_to_client {
            if let Some(key_code) = self.ctrl_pressed_key {
                self.ctrl_forwarded_to_client = true;
                self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
            }
        }
        if self.alt_pressed && !self.alt_forwarded_to_client {
            if let Some(key_code) = self.alt_pressed_key {
                self.alt_forwarded_to_client = true;
                self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
            }
        }
        if self.shift_pressed && !self.shift_forwarded_to_client {
            if let Some(key_code) = self.shift_pressed_key {
                self.shift_forwarded_to_client = true;
                self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
            }
        }
    }

    fn forward_keyboard_key_to_focused_client(
        &mut self,
        key_code: Keycode,
        state: KeyState,
        time: u32,
    ) {
        let can_sample_visible_latency =
            self.presentation == PresentationStatus::Attached && self.active_window().is_some();
        self.stats.record_input(can_sample_visible_latency);
        self.stats.keyboard_events += 1;
        let serial = SERIAL_COUNTER.next_serial();
        self.seat.get_keyboard().expect("keyboard").input::<(), _>(
            self,
            key_code,
            state,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    pub fn forward_pointer_motion(&mut self, pos: Point<f64, Logical>, time: u32) {
        if self.resize_left_panel_to_pointer(pos.x) {
            self.reconfigure_focus_stack();
            self.stats
                .record_input(self.presentation == PresentationStatus::Attached);
            self.stats.pointer_motion_events += 1;
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let hover_surface = self
            .window_under_layout_position(pos)
            .and_then(|(window, _location)| Manifold::window_toplevel_surface(&window));
        self.set_pointer_hover_toplevel_surface(hover_surface);
        let under = self.surface_under(pos);
        self.stats
            .record_input(self.presentation == PresentationStatus::Attached && under.is_some());
        self.stats.pointer_motion_events += 1;
        if under.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }
        let pointer = self.seat.get_pointer().expect("pointer");

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    pub fn forward_pointer_button(&mut self, button_code: u32, state: ButtonState, time: u32) {
        let pointer = self.seat.get_pointer().expect("pointer");
        if button_code == 0x110 {
            match state {
                ButtonState::Pressed
                    if self.left_panel_splitter_hit(
                        pointer.current_location().x,
                        pointer.current_location().y,
                    ) =>
                {
                    self.begin_left_panel_resize();
                    self.record_consumed_pointer_button();
                    return;
                }
                ButtonState::Released if self.end_left_panel_resize() => {
                    self.record_consumed_pointer_button();
                    self.reconfigure_focus_stack();
                    return;
                }
                _ => {}
            }
        }

        let keyboard = self.seat.get_keyboard().expect("keyboard");
        let serial = SERIAL_COUNTER.next_serial();
        let pointer_location = pointer.current_location();
        let target_window = self.window_under_layout_position(pointer_location);
        let target_surface = target_window
            .as_ref()
            .and_then(|(window, _location)| Manifold::window_toplevel_surface(window));
        self.stats.record_input(
            self.presentation == PresentationStatus::Attached && target_window.is_some(),
        );
        self.stats.pointer_button_events += 1;
        if target_window.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }

        match state {
            ButtonState::Pressed if !pointer.is_grabbed() => {
                let focus_surface = target_surface.clone();
                self.log_pointer_click_focus_debug(
                    "pressed-select",
                    button_code,
                    pointer_location,
                    target_surface.as_ref(),
                    focus_surface.as_ref(),
                );
                self.set_pending_pointer_focus_surface(focus_surface);
            }
            ButtonState::Released => {
                let focus_surface = self
                    .take_pending_pointer_focus_surface()
                    .or_else(|| target_surface.clone());
                self.log_pointer_click_focus_debug(
                    "released-select",
                    button_code,
                    pointer_location,
                    target_surface.as_ref(),
                    focus_surface.as_ref(),
                );
                if let Some(surface) = focus_surface.as_ref() {
                    if let Some(window) = self.window_for_toplevel_surface(surface) {
                        self.focus_window(&window);
                    } else {
                        self.focus_toplevel_surface(Some(surface.clone()));
                    }
                } else if !pointer.is_grabbed() {
                    self.activate_window_exclusively(None);
                    keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                }
                self.log_pointer_click_focus_debug(
                    "released-after-focus",
                    button_code,
                    pointer_location,
                    target_surface.as_ref(),
                    focus_surface.as_ref(),
                );
            }
            ButtonState::Pressed => {
                self.log_pointer_click_focus_debug(
                    "pressed-grabbed",
                    button_code,
                    pointer_location,
                    target_surface.as_ref(),
                    None,
                );
            }
        }

        pointer.button(
            self,
            &ButtonEvent {
                button: button_code,
                state,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn record_consumed_pointer_button(&mut self) {
        self.stats
            .record_input(self.presentation == PresentationStatus::Attached);
        self.stats.pointer_button_events += 1;
    }

    pub fn forward_pointer_axis(&mut self, frame: AxisFrame) {
        let pointer = self.seat.get_pointer().expect("pointer");
        let has_target = self.surface_under(pointer.current_location()).is_some();
        self.stats
            .record_input(self.presentation == PresentationStatus::Attached && has_target);
        self.stats.pointer_axis_events += 1;
        if has_target {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

fn built_in_ctrl_layout_action(key_code: Keycode) -> Option<LayoutAction> {
    match key_code {
        KEY_J => Some(LayoutAction::FocusNext),
        KEY_K => Some(LayoutAction::FocusPrevious),
        KEY_L => Some(LayoutAction::SplitActiveRight),
        KEY_H => Some(LayoutAction::SplitActiveDown),
        KEY_S => Some(LayoutAction::SplitActiveSmart),
        KEY_U => Some(LayoutAction::JoinActivePane),
        KEY_DOT => Some(LayoutAction::FocusNextSplit),
        KEY_COMMA => Some(LayoutAction::FocusPreviousSplit),
        _ => None,
    }
}
