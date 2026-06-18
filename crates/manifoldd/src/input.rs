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
    layout::LayoutKeymap,
    state::{InputMode, Manifold, PresentationStatus},
};

const KEY_LEFTCTRL: Keycode = Keycode::new(37);
const KEY_RIGHTCTRL: Keycode = Keycode::new(105);
const KEY_B: Keycode = Keycode::new(56);
const KEY_W: Keycode = Keycode::new(25);

impl Manifold {
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

        if key_code == KEY_B && state == KeyState::Released && self.suppress_mode_toggle_key_release
        {
            self.suppress_mode_toggle_key_release = false;
            self.record_consumed_keyboard_input();
            return;
        }

        if is_ctrl {
            self.ctrl_pressed = state == KeyState::Pressed;
            match (state, self.input_mode, self.ctrl_forwarded_to_client) {
                (KeyState::Pressed, InputMode::Passthrough, _) => {
                    self.ctrl_forwarded_to_client = true;
                    self.forward_keyboard_key_to_focused_client(key_code, state, time);
                }
                (KeyState::Released, _, true) => {
                    self.ctrl_forwarded_to_client = false;
                    self.forward_keyboard_key_to_focused_client(key_code, state, time);
                }
                _ => self.record_consumed_keyboard_input(),
            }
            return;
        }

        if self.ctrl_pressed && key_code == KEY_B {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.suppress_mode_toggle_key_release = true;
                self.toggle_input_mode();
            }
            return;
        }

        if self.input_mode == InputMode::Normal {
            self.record_consumed_keyboard_input();
            if state == KeyState::Pressed {
                self.handle_normal_mode_key(key_code);
            }
            return;
        }

        self.forward_keyboard_key_to_focused_client(key_code, state, time);
    }

    fn handle_normal_mode_key(&mut self, key_code: Keycode) {
        if let Some(result) = self.wasm_key_handler.as_mut().map(|handler| {
            handler.handle_key(
                key_code,
                KeyState::Pressed,
                self.input_mode,
                self.ctrl_pressed,
            )
        }) {
            match result {
                Ok(outcome) => {
                    outcome.apply(self);
                    return;
                }
                Err(err) => {
                    warn!(
                        ?err,
                        "WASM normal-mode key handler failed; using built-in fallback"
                    );
                }
            }
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

    fn record_consumed_keyboard_input(&mut self) {
        let can_sample_visible_latency =
            self.presentation == PresentationStatus::Attached && self.active_window().is_some();
        self.stats.record_input(can_sample_visible_latency);
        self.stats.keyboard_events += 1;
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
        let serial = SERIAL_COUNTER.next_serial();
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
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        let serial = SERIAL_COUNTER.next_serial();
        let target_window: Option<(smithay::desktop::Window, Point<i32, Logical>)> = self
            .active_window()
            .filter(|window| {
                window
                    .surface_under(
                        pointer.current_location(),
                        smithay::desktop::WindowSurfaceType::ALL,
                    )
                    .is_some()
            })
            .map(|window| (window, Point::<i32, Logical>::from((0, 0))));
        self.stats.record_input(
            self.presentation == PresentationStatus::Attached && target_window.is_some(),
        );
        self.stats.pointer_button_events += 1;
        if target_window.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }

        if state == ButtonState::Pressed && !pointer.is_grabbed() {
            if let Some((window, _location)) = target_window {
                self.focus_window(&window);
            } else {
                self.activate_window_exclusively(None);
                keyboard.set_focus(self, Option::<WlSurface>::None, serial);
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
