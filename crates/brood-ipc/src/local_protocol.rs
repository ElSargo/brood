//! Local fixed-record codec for transport-neutral client messages.

use brood_protocol::{ClientLifecycle, ClientMessage, InputEvent, Lane, PanePosition};
use thiserror::Error;

use crate::{
    CriticalRecord, INPUT_CLIENT_DISCONNECT, INPUT_FRAME_PRESENTED, INPUT_KEYBOARD_KEY,
    INPUT_KEYBOARD_RESET, INPUT_PRESENTATION_FOCUS, INPUT_PRESENTATION_SIZE, INPUT_WINDOW_FOCUS,
    INPUT_WINDOW_POINTER_AXIS, INPUT_WINDOW_POINTER_BUTTON, INPUT_WINDOW_POINTER_LEAVE,
    INPUT_WINDOW_POINTER_MOTION, INPUT_WINDOW_POINTER_WHEEL, INPUT_WINDOW_RESIZE,
    decode_i32_argument, encode_i32_argument,
};

/// A semantic message cannot be represented by the local fixed-record ABI.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LocalProtocolError {
    #[error("message belongs on a different local presentation lane")]
    WrongLane,
    #[error("local presentation record contains an unknown opcode")]
    UnknownOpcode,
    #[error("local presentation record contains an out-of-range value")]
    InvalidValue,
}

/// Encode one transport-neutral client message into the local critical lane.
pub fn encode_client_record(
    message: ClientMessage,
    connection_generation: u64,
    sequence: u64,
) -> Result<CriticalRecord, LocalProtocolError> {
    let (opcode, arguments) = match message {
        ClientMessage::Input(input) => encode_input(&input),
        ClientMessage::Lifecycle(ClientLifecycle::FramePresented { sequence }) => {
            (INPUT_FRAME_PRESENTED, [sequence, 0, 0, 0, 0])
        }
        ClientMessage::Lifecycle(ClientLifecycle::Disconnect) => (INPUT_CLIENT_DISCONNECT, [0; 5]),
        ClientMessage::Actor(_)
        | ClientMessage::BackendRequest(_)
        | ClientMessage::ReleaseResource { .. } => {
            return Err(LocalProtocolError::WrongLane);
        }
    };
    Ok(CriticalRecord {
        opcode,
        connection_generation,
        sequence,
        arguments,
        ..CriticalRecord::default()
    })
}

/// Decode one local critical-lane record into its semantic client message.
pub fn decode_client_record(record: &CriticalRecord) -> Result<ClientMessage, LocalProtocolError> {
    let input = match record.opcode {
        INPUT_KEYBOARD_KEY => InputEvent::KeyboardKey {
            keycode: u32::try_from(record.arguments[0])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            pressed: record.arguments[1] != 0,
        },
        INPUT_KEYBOARD_RESET => InputEvent::KeyboardReset,
        INPUT_PRESENTATION_SIZE => InputEvent::PresentationSize {
            width: u32::try_from(record.arguments[0])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            height: u32::try_from(record.arguments[1])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            refresh_millihz: u32::try_from(record.arguments[2])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
        },
        INPUT_PRESENTATION_FOCUS => InputEvent::PresentationFocus {
            focused: record.arguments[0] != 0,
        },
        INPUT_WINDOW_FOCUS => InputEvent::WindowFocus {
            pane: record.arguments[0],
        },
        INPUT_WINDOW_RESIZE => InputEvent::WindowResize {
            pane: record.arguments[0],
            width: u32::try_from(record.arguments[1])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            height: u32::try_from(record.arguments[2])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
        },
        INPUT_WINDOW_POINTER_MOTION => InputEvent::PointerMotion {
            position: position(record),
        },
        INPUT_WINDOW_POINTER_BUTTON => InputEvent::PointerButton {
            position: PanePosition {
                pane: record.arguments[0],
                x: f64::from_bits(record.arguments[3]),
                y: f64::from_bits(record.arguments[4]),
            },
            button: u32::try_from(record.arguments[1])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            pressed: record.arguments[2] != 0,
        },
        INPUT_WINDOW_POINTER_AXIS => InputEvent::PointerAxis {
            position: PanePosition {
                pane: record.arguments[0],
                x: f64::from_bits(record.arguments[3]),
                y: f64::from_bits(record.arguments[4]),
            },
            horizontal: f64::from_bits(record.arguments[1]),
            vertical: f64::from_bits(record.arguments[2]),
        },
        INPUT_WINDOW_POINTER_WHEEL => InputEvent::PointerWheel {
            position: PanePosition {
                pane: record.arguments[0],
                x: f64::from_bits(record.arguments[3]),
                y: f64::from_bits(record.arguments[4]),
            },
            horizontal_v120: decode_i32_argument(record.arguments[1]),
            vertical_v120: decode_i32_argument(record.arguments[2]),
        },
        INPUT_WINDOW_POINTER_LEAVE => InputEvent::PointerLeave,
        INPUT_FRAME_PRESENTED => {
            return Ok(ClientMessage::Lifecycle(ClientLifecycle::FramePresented {
                sequence: record.arguments[0],
            }));
        }
        INPUT_CLIENT_DISCONNECT => {
            return Ok(ClientMessage::Lifecycle(ClientLifecycle::Disconnect));
        }
        _ => return Err(LocalProtocolError::UnknownOpcode),
    };
    Ok(ClientMessage::Input(input))
}

/// Whether a local record belongs to a supersedable semantic lane.
#[must_use]
pub fn is_coalescible_client_record(record: &CriticalRecord) -> bool {
    decode_client_record(record).is_ok_and(|message| {
        matches!(message, ClientMessage::Input(ref input) if input.lane() == Lane::LatestState)
    })
}

fn encode_input(input: &InputEvent) -> (u32, [u64; 5]) {
    match input {
        InputEvent::KeyboardKey { keycode, pressed } => (
            INPUT_KEYBOARD_KEY,
            [u64::from(*keycode), u64::from(*pressed), 0, 0, 0],
        ),
        InputEvent::KeyboardReset => (INPUT_KEYBOARD_RESET, [0; 5]),
        InputEvent::PresentationSize {
            width,
            height,
            refresh_millihz,
        } => (
            INPUT_PRESENTATION_SIZE,
            [
                u64::from(*width),
                u64::from(*height),
                u64::from(*refresh_millihz),
                0,
                0,
            ],
        ),
        InputEvent::PresentationFocus { focused } => {
            (INPUT_PRESENTATION_FOCUS, [u64::from(*focused), 0, 0, 0, 0])
        }
        InputEvent::WindowFocus { pane } => (INPUT_WINDOW_FOCUS, [*pane, 0, 0, 0, 0]),
        InputEvent::WindowResize {
            pane,
            width,
            height,
        } => (
            INPUT_WINDOW_RESIZE,
            [*pane, u64::from(*width), u64::from(*height), 0, 0],
        ),
        InputEvent::PointerMotion { position } => (
            INPUT_WINDOW_POINTER_MOTION,
            [
                position.pane,
                position.x.to_bits(),
                position.y.to_bits(),
                0,
                0,
            ],
        ),
        InputEvent::PointerButton {
            position,
            button,
            pressed,
        } => (
            INPUT_WINDOW_POINTER_BUTTON,
            [
                position.pane,
                u64::from(*button),
                u64::from(*pressed),
                position.x.to_bits(),
                position.y.to_bits(),
            ],
        ),
        InputEvent::PointerAxis {
            position,
            horizontal,
            vertical,
        } => (
            INPUT_WINDOW_POINTER_AXIS,
            [
                position.pane,
                horizontal.to_bits(),
                vertical.to_bits(),
                position.x.to_bits(),
                position.y.to_bits(),
            ],
        ),
        InputEvent::PointerWheel {
            position,
            horizontal_v120,
            vertical_v120,
        } => (
            INPUT_WINDOW_POINTER_WHEEL,
            [
                position.pane,
                encode_i32_argument(*horizontal_v120),
                encode_i32_argument(*vertical_v120),
                position.x.to_bits(),
                position.y.to_bits(),
            ],
        ),
        InputEvent::PointerLeave => (INPUT_WINDOW_POINTER_LEAVE, [0; 5]),
    }
}

const fn position(record: &CriticalRecord) -> PanePosition {
    PanePosition {
        pane: record.arguments[0],
        x: f64::from_bits(record.arguments[1]),
        y: f64::from_bits(record.arguments[2]),
    }
}

#[cfg(test)]
mod tests {
    use brood_protocol::{ClientLifecycle, ClientMessage, InputEvent, PanePosition};

    use super::{decode_client_record, encode_client_record, is_coalescible_client_record};

    #[test]
    fn semantic_input_round_trips_through_local_record() {
        let messages = [
            ClientMessage::Input(InputEvent::KeyboardKey {
                keycode: 44,
                pressed: true,
            }),
            ClientMessage::Input(InputEvent::PointerButton {
                position: PanePosition {
                    pane: 9,
                    x: 12.5,
                    y: 20.25,
                },
                button: 0x110,
                pressed: false,
            }),
            ClientMessage::Input(InputEvent::PointerWheel {
                position: PanePosition {
                    pane: 3,
                    x: -1.0,
                    y: 2.0,
                },
                horizontal_v120: -120,
                vertical_v120: 240,
            }),
            ClientMessage::Lifecycle(ClientLifecycle::FramePresented { sequence: 99 }),
        ];
        for (sequence, message) in messages.iter().enumerate() {
            let result = encode_client_record(message.clone(), 7, sequence as u64);
            assert!(result.is_ok());
            let Some(record) = result.ok() else {
                return;
            };
            assert_eq!(decode_client_record(&record), Ok(message.clone()));
        }
    }

    #[test]
    fn only_pointer_motion_is_coalescible() {
        let motion = encode_client_record(
            ClientMessage::Input(InputEvent::PointerMotion {
                position: PanePosition {
                    pane: 1,
                    x: 2.0,
                    y: 3.0,
                },
            }),
            1,
            1,
        );
        let button = encode_client_record(
            ClientMessage::Input(InputEvent::PointerButton {
                position: PanePosition {
                    pane: 1,
                    x: 2.0,
                    y: 3.0,
                },
                button: 1,
                pressed: true,
            }),
            1,
            2,
        );
        assert!(motion.is_ok());
        assert!(button.is_ok());
        let Some(motion) = motion.ok() else {
            return;
        };
        let Some(button) = button.ok() else {
            return;
        };
        assert!(is_coalescible_client_record(&motion));
        assert!(!is_coalescible_client_record(&button));
    }
}
