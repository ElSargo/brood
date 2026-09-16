use crate::{geometry::finite_axis_value, state::Brood};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};

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

/// Whether a managed presentation pointer button event should reach Smithay.
///
/// Presses without a target are dropped; releases still forward so implicit
/// click / DnD grabs cannot stick to a destroyed surface.
pub const fn should_forward_managed_pointer_button(
    target_exists: bool,
    state: ButtonState,
) -> bool {
    target_exists || matches!(state, ButtonState::Released)
}

impl Brood {
    pub fn forward_keyboard_key(&mut self, key_code: Keycode, state: KeyState, time: u32) {
        self.forward_keyboard_key_to_focused_client(key_code, state, time);
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
            self.dmabuf.fast_path.is_some() && self.active_window().is_some();
        self.stats.record_input(can_sample_visible_latency);
        self.stats.keyboard_events += 1;
        let Some(keyboard) = self.seat.get_keyboard() else {
            tracing::warn!("dropping keyboard key: seat has no keyboard");
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.input::<(), _>(self, key_code, state, serial, time, |_, _, _| {
            FilterResult::Forward
        });
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
            .record_input(self.dmabuf.fast_path.is_some() && under.is_some());
        self.stats.pointer_motion_events += 1;
        if under.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            tracing::warn!(pane, "dropping pointer motion: seat has no pointer");
            return;
        };
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
        let Some(pointer) = self.seat.get_pointer() else {
            tracing::warn!("dropping pointer leave: seat has no pointer");
            return;
        };
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

    pub fn forward_pointer_button_to_pane(
        &mut self,
        pane: u64,
        button_code: u32,
        state: ButtonState,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            tracing::warn!(pane, "dropping pointer button: seat has no pointer");
            return;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            tracing::warn!(pane, "dropping pointer button: seat has no keyboard");
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let target_window = self.presentation_window_for_pane(pane);
        let target_surface = target_window
            .as_ref()
            .and_then(Self::window_toplevel_surface);
        self.stats
            .record_input(self.dmabuf.fast_path.is_some() && target_window.is_some());
        self.stats.pointer_button_events += 1;
        if target_window.is_some() {
            self.stats.pointer_target_hits += 1;
        } else {
            self.stats.pointer_target_misses += 1;
        }
        if !should_forward_managed_pointer_button(target_window.is_some(), state) {
            return;
        }

        match (state, target_window.is_some()) {
            (ButtonState::Pressed, _) if !pointer.is_grabbed() => {
                self.set_pending_pointer_focus_surface(target_surface);
            }
            (ButtonState::Released, true) => {
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
            (ButtonState::Released, false) => {
                // A pane can disappear between a button press and release. The
                // release must still reach Smithay so its implicit click or DnD
                // grab cannot remain attached to the destroyed surface.
                self.take_pending_pointer_focus_surface();
            }
            (ButtonState::Pressed, _) => {}
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

    pub fn forward_pointer_axis_to_pane(&mut self, pane: u64, frame: AxisFrame) {
        let Some(pointer) = self.seat.get_pointer() else {
            tracing::warn!(pane, "dropping pointer axis: seat has no pointer");
            return;
        };
        let has_target = self.presentation_window_for_pane(pane).is_some();
        self.stats
            .record_input(self.dmabuf.fast_path.is_some() && has_target);
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
