#![allow(
    clippy::expect_used,
    reason = "the compositor creates its keyboard and pointer seats before input dispatch"
)]

use crate::{
    geometry::{finite_axis_value, truncating_axis_v120},
    state::{InputMode, Manifold, PresentationStatus},
};
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

const KEY_LEFTCTRL: Keycode = Keycode::new(37);
const KEY_RIGHTCTRL: Keycode = Keycode::new(105);
const KEY_LEFTSHIFT: Keycode = Keycode::new(50);
const KEY_RIGHTSHIFT: Keycode = Keycode::new(62);
const KEY_LEFTALT: Keycode = Keycode::new(64);
const KEY_RIGHTALT: Keycode = Keycode::new(108);

pub fn managed_continuous_axis_frame(time: u32, horizontal: f64, vertical: f64) -> AxisFrame {
    let mut frame = AxisFrame::new(time).source(AxisSource::Continuous);
    if let Some(horizontal) = finite_axis_value(horizontal).filter(|value| *value != 0.0) {
        frame = frame.value(Axis::Horizontal, horizontal);
    }
    if let Some(vertical) = finite_axis_value(vertical).filter(|value| *value != 0.0) {
        frame = frame.value(Axis::Vertical, vertical);
    }
    frame
}

pub fn managed_wheel_axis_frame(time: u32, horizontal_v120: i32, vertical_v120: i32) -> AxisFrame {
    let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
    if horizontal_v120 != 0 {
        frame = frame
            .value(Axis::Horizontal, f64::from(horizontal_v120) * 15.0 / 120.0)
            .v120(Axis::Horizontal, horizontal_v120);
    }
    if vertical_v120 != 0 {
        frame = frame
            .value(Axis::Vertical, f64::from(vertical_v120) * 15.0 / 120.0)
            .v120(Axis::Vertical, vertical_v120);
    }
    frame
}

impl Manifold {
    #[allow(
        dead_code,
        reason = "real libinput backends call this generic dispatch path"
    )]
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
                let horizontal_amount =
                    finite_axis_value(event.amount(Axis::Horizontal).unwrap_or_else(|| {
                        event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0
                    }));
                let vertical_amount =
                    finite_axis_value(event.amount(Axis::Vertical).unwrap_or_else(|| {
                        event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0
                    }));
                let horizontal_discrete = event
                    .amount_v120(Axis::Horizontal)
                    .and_then(truncating_axis_v120);
                let vertical_discrete = event
                    .amount_v120(Axis::Vertical)
                    .and_then(truncating_axis_v120);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if let Some(horizontal_amount) = horizontal_amount.filter(|amount| *amount != 0.0) {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete);
                    }
                }
                if let Some(vertical_amount) = vertical_amount.filter(|amount| *amount != 0.0) {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_discrete {
                        frame = frame.v120(Axis::Vertical, discrete);
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

        if self.try_wasm_global_key_policy(key_code, state) == Some(true) {
            self.record_consumed_keyboard_input();
            return;
        }

        if self.shortcut_passthrough() || self.input_mode != InputMode::Normal {
            self.forward_pressed_modifiers_to_focused_client(time);
        }
        self.forward_keyboard_key_to_focused_client(key_code, state, time);
    }

    fn record_consumed_keyboard_input(&mut self) {
        let can_sample_visible_latency =
            self.presentation == PresentationStatus::Attached && self.active_window().is_some();
        self.stats.record_input(can_sample_visible_latency);
        self.stats.keyboard_events += 1;
    }

    fn forward_pressed_modifiers_to_focused_client(&mut self, time: u32) {
        if self.ctrl_pressed
            && !self.ctrl_forwarded_to_client
            && let Some(key_code) = self.ctrl_pressed_key
        {
            self.ctrl_forwarded_to_client = true;
            self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
        }
        if self.alt_pressed
            && !self.alt_forwarded_to_client
            && let Some(key_code) = self.alt_pressed_key
        {
            self.alt_forwarded_to_client = true;
            self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
        }
        if self.shift_pressed
            && !self.shift_forwarded_to_client
            && let Some(key_code) = self.shift_pressed_key
        {
            self.shift_forwarded_to_client = true;
            self.forward_keyboard_key_to_focused_client(key_code, KeyState::Pressed, time);
        }
    }

    fn forward_keyboard_key_to_focused_client(
        &mut self,
        key_code: Keycode,
        state: KeyState,
        time: u32,
    ) {
        match state {
            KeyState::Pressed => {
                if !self.forwarded_pressed_keys.contains(&key_code) {
                    self.forwarded_pressed_keys.push(key_code);
                }
            }
            KeyState::Released => {
                self.forwarded_pressed_keys
                    .retain(|pressed_key| *pressed_key != key_code);
            }
        }
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
            .and_then(|(window, _location)| Self::window_toplevel_surface(&window));
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

    pub fn forward_pointer_motion_to_pane(
        &mut self,
        pane: u64,
        pos: Point<f64, Logical>,
        time: u32,
    ) {
        let serial = SERIAL_COUNTER.next_serial();
        let target_window = self.presentation_window_for_pane(pane);
        let hover_surface = target_window
            .as_ref()
            .and_then(Self::window_toplevel_surface);
        self.set_pointer_hover_toplevel_surface(hover_surface);
        let under = self.presentation_surface_under_pane_local(pane, pos);
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

    pub fn forward_pointer_leave_managed_pane(&mut self, time: u32) {
        self.set_pointer_hover_toplevel_surface(None);
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().expect("pointer");
        let location = pointer.current_location();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location,
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
            .and_then(|(window, _location)| Self::window_toplevel_surface(window));
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

    pub fn forward_pointer_button_to_pane(
        &mut self,
        pane: u64,
        button_code: u32,
        state: ButtonState,
        time: u32,
    ) {
        let pointer = self.seat.get_pointer().expect("pointer");
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        let serial = SERIAL_COUNTER.next_serial();
        let target_window = self.presentation_window_for_pane(pane);
        let target_surface = target_window
            .as_ref()
            .and_then(Self::window_toplevel_surface);
        self.stats.record_input(
            self.presentation == PresentationStatus::Attached && target_window.is_some(),
        );
        self.stats.pointer_button_events += 1;
        if target_window.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
            return;
        }

        match state {
            ButtonState::Pressed if !pointer.is_grabbed() => {
                self.set_pending_pointer_focus_surface(target_surface);
            }
            ButtonState::Released => {
                let focus_surface = self
                    .take_pending_pointer_focus_surface()
                    .or_else(|| target_surface.clone());
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
            }
            ButtonState::Pressed => {}
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
        tracing::info!(
            pane,
            button_code,
            ?state,
            grabbed = pointer.is_grabbed(),
            "forwarded presentation pointer button"
        );
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

    pub fn forward_pointer_axis_to_pane(&mut self, pane: u64, frame: AxisFrame) {
        let pointer = self.seat.get_pointer().expect("pointer");
        let has_target = self.presentation_window_for_pane(pane).is_some();
        self.stats
            .record_input(self.presentation == PresentationStatus::Attached && has_target);
        self.stats.pointer_axis_events += 1;
        if has_target {
            self.stats.pointer_target_hits += 1;
            tracing::info!(
                pane,
                grabbed = pointer.is_grabbed(),
                "forwarded presentation pointer axis"
            );
            pointer.axis(self, frame);
            pointer.frame(self);
        } else {
            self.stats.pointer_target_misses += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_wheel_frame_preserves_v120_and_legacy_axis_values() {
        let frame = managed_wheel_axis_frame(7, -120, 60);
        assert_eq!(frame.source, Some(AxisSource::Wheel));
        assert_eq!(frame.v120, Some((-120, 60)));
        assert_eq!(frame.axis, (-15.0, 7.5));
    }

    #[test]
    fn managed_continuous_frame_rejects_non_finite_values() {
        let frame = managed_continuous_axis_frame(9, f64::NAN, -2.5);
        assert_eq!(frame.source, Some(AxisSource::Continuous));
        assert_eq!(frame.v120, None);
        assert_eq!(frame.axis, (0.0, -2.5));
    }
}
