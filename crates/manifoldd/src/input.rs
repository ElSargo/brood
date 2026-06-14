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

use crate::state::{Manifold, PresentationStatus};

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
        let can_sample_visible_latency = self.presentation == PresentationStatus::Attached
            && self.space.elements().next().is_some();
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
        let target_window = self
            .space
            .element_under(pointer.current_location())
            .map(|(window, location)| (window.clone(), location));
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
                self.space.raise_element(&window, true);
                if let Some(active_surface) = Manifold::window_toplevel_surface(&window) {
                    self.activate_window_exclusively(Some(&active_surface));
                    keyboard.set_focus(self, Some(active_surface), serial);
                } else {
                    self.activate_window_exclusively(None);
                    keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                }
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
