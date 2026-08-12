//! Local fixed-record codec for transport-neutral client messages.

use brood_protocol::{
    ClientLifecycle, ClientMessage, InputEvent, Lane, ManagedResizeRejectionReason,
    ManagedResizeStatus, ManagedResizeStatusResult, PanePosition,
};
use thiserror::Error;

use crate::{
    CriticalRecord, FRAME_MANAGED_RESIZE_STATUS, FRAME_WINDOW_DMABUF, FRAME_WINDOW_FLAG_ACTIVE,
    FRAME_WINDOW_FLAGS, FRAME_WINDOW_SHM, INPUT_CLIENT_DISCONNECT, INPUT_FRAME_PRESENTED,
    INPUT_KEYBOARD_KEY, INPUT_KEYBOARD_RESET, INPUT_PRESENTATION_FOCUS, INPUT_PRESENTATION_SIZE,
    INPUT_RELEASE_RESOURCE, INPUT_WINDOW_FOCUS, INPUT_WINDOW_POINTER_AXIS,
    INPUT_WINDOW_POINTER_BUTTON, INPUT_WINDOW_POINTER_LEAVE, INPUT_WINDOW_POINTER_MOTION,
    INPUT_WINDOW_POINTER_WHEEL, INPUT_WINDOW_RESIZE, decode_i32_argument, decode_i32_pair,
    decode_u32_pair, encode_i32_argument, encode_i32_pair, encode_u32_pair,
};

const MANAGED_RESIZE_CONFIGURED: u32 = 1;
const MANAGED_RESIZE_PANE_UNAVAILABLE: u32 = 2;
const MANAGED_RESIZE_CORRELATION_CAPACITY: u32 = 3;
const MANAGED_RESIZE_CONFIGURE_FAILED: u32 = 4;
const MANAGED_RESIZE_SIZE_UNSATISFIED: u32 = 5;

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

/// Decoded fields common to local DMA-BUF and shared-memory window records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalWindowFrame {
    pub pane: u64,
    pub resource: u64,
    pub source: [i32; 4],
    pub active: bool,
    pub resize_epoch: Option<u64>,
}

/// Encode one transport-neutral client message into the local critical lane.
pub fn encode_client_record(
    message: ClientMessage,
    connection_generation: u64,
    sequence: u64,
) -> Result<CriticalRecord, LocalProtocolError> {
    let (opcode, arguments) = match message {
        ClientMessage::Input(input) => {
            validate_local_input(&input)?;
            encode_input(&input)
        }
        ClientMessage::Lifecycle(ClientLifecycle::FramePresented { sequence }) => {
            (INPUT_FRAME_PRESENTED, [sequence, 0, 0, 0, 0])
        }
        ClientMessage::Lifecycle(ClientLifecycle::Disconnect) => (INPUT_CLIENT_DISCONNECT, [0; 5]),
        ClientMessage::ReleaseResource { resource } => {
            (INPUT_RELEASE_RESOURCE, [resource, 0, 0, 0, 0])
        }
        ClientMessage::Actor(_) | ClientMessage::BackendRequest(_) | ClientMessage::Artifact(_) => {
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
            width: nonzero_u32(record.arguments[0])?,
            height: nonzero_u32(record.arguments[1])?,
            refresh_millihz: u32::try_from(record.arguments[2])
                .map_err(|_error| LocalProtocolError::InvalidValue)?,
            scale_120: nonzero_u32(record.arguments[3])?,
        },
        INPUT_PRESENTATION_FOCUS => InputEvent::PresentationFocus {
            focused: record.arguments[0] != 0,
        },
        INPUT_WINDOW_FOCUS => InputEvent::WindowFocus {
            pane: record.arguments[0],
        },
        INPUT_WINDOW_RESIZE => InputEvent::WindowResize {
            pane: nonzero_u64(record.arguments[0])?,
            logical_width: nonzero_pair(record.arguments[1])?.0,
            logical_height: nonzero_pair(record.arguments[1])?.1,
            pixel_width: nonzero_pair(record.arguments[2])?.0,
            pixel_height: nonzero_pair(record.arguments[2])?.1,
            scale_120: nonzero_u32(record.arguments[3])?,
            resize_epoch: (record.arguments[4] != 0).then_some(record.arguments[4]),
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
        INPUT_RELEASE_RESOURCE => {
            return Ok(ClientMessage::ReleaseResource {
                resource: nonzero_u64(record.arguments[0])?,
            });
        }
        _ => return Err(LocalProtocolError::UnknownOpcode),
    };
    Ok(ClientMessage::Input(input))
}

/// Encode one managed-window record for a local scene batch.
pub fn encode_window_frame_record(
    opcode: u32,
    connection_generation: u64,
    sequence: u64,
    frame: LocalWindowFrame,
) -> Result<CriticalRecord, LocalProtocolError> {
    if !matches!(opcode, FRAME_WINDOW_DMABUF | FRAME_WINDOW_SHM)
        || frame.pane == 0
        || frame.resource == 0
        || frame.source[2] <= 0
        || frame.source[3] <= 0
        || frame.resize_epoch == Some(0)
    {
        return Err(LocalProtocolError::InvalidValue);
    }
    Ok(CriticalRecord {
        opcode,
        flags: u32::from(frame.active) * FRAME_WINDOW_FLAG_ACTIVE,
        connection_generation,
        sequence,
        arguments: [
            frame.pane,
            frame.resource,
            encode_i32_pair(frame.source[0], frame.source[1]),
            encode_i32_pair(frame.source[2], frame.source[3]),
            frame.resize_epoch.unwrap_or_default(),
        ],
    })
}

/// Decode one local managed-window record and reject reserved flag bits.
pub fn decode_window_frame_record(
    record: &CriticalRecord,
) -> Result<LocalWindowFrame, LocalProtocolError> {
    if !matches!(record.opcode, FRAME_WINDOW_DMABUF | FRAME_WINDOW_SHM)
        || record.flags & !FRAME_WINDOW_FLAGS != 0
        || record.arguments[0] == 0
        || record.arguments[1] == 0
    {
        return Err(LocalProtocolError::InvalidValue);
    }
    let (source_x, source_y) = decode_i32_pair(record.arguments[2]);
    let (source_width, source_height) = decode_i32_pair(record.arguments[3]);
    if source_width <= 0 || source_height <= 0 {
        return Err(LocalProtocolError::InvalidValue);
    }
    Ok(LocalWindowFrame {
        pane: record.arguments[0],
        resource: record.arguments[1],
        source: [source_x, source_y, source_width, source_height],
        active: record.flags & FRAME_WINDOW_FLAG_ACTIVE != 0,
        resize_epoch: (record.arguments[4] != 0).then_some(record.arguments[4]),
    })
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
            scale_120,
        } => (
            INPUT_PRESENTATION_SIZE,
            [
                u64::from(*width),
                u64::from(*height),
                u64::from(*refresh_millihz),
                u64::from(*scale_120),
                0,
            ],
        ),
        InputEvent::PresentationFocus { focused } => {
            (INPUT_PRESENTATION_FOCUS, [u64::from(*focused), 0, 0, 0, 0])
        }
        InputEvent::WindowFocus { pane } => (INPUT_WINDOW_FOCUS, [*pane, 0, 0, 0, 0]),
        InputEvent::WindowResize {
            pane,
            logical_width,
            logical_height,
            pixel_width,
            pixel_height,
            scale_120,
            resize_epoch,
        } => (
            INPUT_WINDOW_RESIZE,
            [
                *pane,
                encode_u32_pair(*logical_width, *logical_height),
                encode_u32_pair(*pixel_width, *pixel_height),
                u64::from(*scale_120),
                resize_epoch.unwrap_or_default(),
            ],
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

fn nonzero_u32(value: u64) -> Result<u32, LocalProtocolError> {
    let value = u32::try_from(value).map_err(|_error| LocalProtocolError::InvalidValue)?;
    (value != 0)
        .then_some(value)
        .ok_or(LocalProtocolError::InvalidValue)
}

fn nonzero_u64(value: u64) -> Result<u64, LocalProtocolError> {
    (value != 0)
        .then_some(value)
        .ok_or(LocalProtocolError::InvalidValue)
}

fn validate_local_input(input: &InputEvent) -> Result<(), LocalProtocolError> {
    let valid = match input {
        InputEvent::PresentationSize {
            width,
            height,
            scale_120,
            ..
        } => *width != 0 && *height != 0 && *scale_120 != 0,
        InputEvent::WindowResize {
            pane,
            logical_width,
            logical_height,
            pixel_width,
            pixel_height,
            scale_120,
            resize_epoch,
        } => {
            *pane != 0
                && *logical_width != 0
                && *logical_height != 0
                && *pixel_width != 0
                && *pixel_height != 0
                && *scale_120 != 0
                && *resize_epoch != Some(0)
        }
        InputEvent::KeyboardKey { .. }
        | InputEvent::KeyboardReset
        | InputEvent::PresentationFocus { .. }
        | InputEvent::WindowFocus { .. }
        | InputEvent::PointerMotion { .. }
        | InputEvent::PointerButton { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::PointerWheel { .. }
        | InputEvent::PointerLeave => true,
    };
    valid.then_some(()).ok_or(LocalProtocolError::InvalidValue)
}

fn nonzero_pair(value: u64) -> Result<(u32, u32), LocalProtocolError> {
    let pair = decode_u32_pair(value);
    (pair.0 != 0 && pair.1 != 0)
        .then_some(pair)
        .ok_or(LocalProtocolError::InvalidValue)
}

/// Encode one managed resize admission status into the local critical lane.
pub fn encode_managed_resize_status_record(
    status: ManagedResizeStatus,
    connection_generation: u64,
    sequence: u64,
) -> Result<CriticalRecord, LocalProtocolError> {
    if status.pane == 0 || status.resize_epoch == 0 {
        return Err(LocalProtocolError::InvalidValue);
    }
    let (flags, logical_size, pixel_size, scale_120) = match status.result {
        ManagedResizeStatusResult::Configured {
            logical_size,
            pixel_size,
            scale_120,
        } => {
            if logical_size.contains(&0) || pixel_size.contains(&0) || scale_120 == 0 {
                return Err(LocalProtocolError::InvalidValue);
            }
            (
                MANAGED_RESIZE_CONFIGURED,
                logical_size,
                pixel_size,
                scale_120,
            )
        }
        ManagedResizeStatusResult::Rejected { reason } => {
            let flags = match reason {
                ManagedResizeRejectionReason::PaneUnavailable => MANAGED_RESIZE_PANE_UNAVAILABLE,
                ManagedResizeRejectionReason::CorrelationCapacity => {
                    MANAGED_RESIZE_CORRELATION_CAPACITY
                }
                ManagedResizeRejectionReason::ConfigureFailed => MANAGED_RESIZE_CONFIGURE_FAILED,
                ManagedResizeRejectionReason::SizeUnsatisfied => MANAGED_RESIZE_SIZE_UNSATISFIED,
            };
            (flags, [0; 2], [0; 2], 0)
        }
    };
    Ok(CriticalRecord {
        opcode: FRAME_MANAGED_RESIZE_STATUS,
        flags,
        connection_generation,
        sequence,
        arguments: [
            status.pane,
            status.resize_epoch,
            encode_u32_pair(logical_size[0], logical_size[1]),
            encode_u32_pair(pixel_size[0], pixel_size[1]),
            u64::from(scale_120),
        ],
    })
}

/// Decode one local managed resize admission status.
pub fn decode_managed_resize_status_record(
    record: &CriticalRecord,
) -> Result<ManagedResizeStatus, LocalProtocolError> {
    if record.opcode != FRAME_MANAGED_RESIZE_STATUS
        || record.arguments[0] == 0
        || record.arguments[1] == 0
    {
        return Err(LocalProtocolError::InvalidValue);
    }
    let result = match record.flags {
        MANAGED_RESIZE_CONFIGURED => {
            let logical_size = nonzero_pair(record.arguments[2])?;
            let pixel_size = nonzero_pair(record.arguments[3])?;
            ManagedResizeStatusResult::Configured {
                logical_size: logical_size.into(),
                pixel_size: pixel_size.into(),
                scale_120: nonzero_u32(record.arguments[4])?,
            }
        }
        MANAGED_RESIZE_PANE_UNAVAILABLE => {
            rejected_status(record, ManagedResizeRejectionReason::PaneUnavailable)?
        }
        MANAGED_RESIZE_CORRELATION_CAPACITY => {
            rejected_status(record, ManagedResizeRejectionReason::CorrelationCapacity)?
        }
        MANAGED_RESIZE_CONFIGURE_FAILED => {
            rejected_status(record, ManagedResizeRejectionReason::ConfigureFailed)?
        }
        MANAGED_RESIZE_SIZE_UNSATISFIED => {
            rejected_status(record, ManagedResizeRejectionReason::SizeUnsatisfied)?
        }
        _ => return Err(LocalProtocolError::InvalidValue),
    };
    Ok(ManagedResizeStatus {
        pane: record.arguments[0],
        resize_epoch: record.arguments[1],
        result,
    })
}

fn rejected_status(
    record: &CriticalRecord,
    reason: ManagedResizeRejectionReason,
) -> Result<ManagedResizeStatusResult, LocalProtocolError> {
    if record.arguments[2..].iter().any(|value| *value != 0) {
        return Err(LocalProtocolError::InvalidValue);
    }
    Ok(ManagedResizeStatusResult::Rejected { reason })
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
    use brood_protocol::{
        ClientLifecycle, ClientMessage, InputEvent, ManagedResizeRejectionReason,
        ManagedResizeStatus, ManagedResizeStatusResult, PanePosition,
    };

    use crate::{CriticalRecord, FRAME_WINDOW_DMABUF};

    use super::{
        LocalProtocolError, LocalWindowFrame, decode_client_record,
        decode_managed_resize_status_record, decode_window_frame_record, encode_client_record,
        encode_managed_resize_status_record, encode_window_frame_record,
        is_coalescible_client_record,
    };

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
            ClientMessage::Input(InputEvent::PresentationSize {
                width: 1_920,
                height: 1_080,
                refresh_millihz: 60_000,
                scale_120: 180,
            }),
            ClientMessage::Input(InputEvent::WindowResize {
                pane: 7,
                logical_width: 200,
                logical_height: 120,
                pixel_width: 300,
                pixel_height: 180,
                scale_120: 180,
                resize_epoch: Some(41),
            }),
            ClientMessage::Input(InputEvent::WindowResize {
                pane: 7,
                logical_width: 200,
                logical_height: 120,
                pixel_width: 300,
                pixel_height: 180,
                scale_120: 180,
                resize_epoch: None,
            }),
            ClientMessage::Lifecycle(ClientLifecycle::FramePresented { sequence: 99 }),
            ClientMessage::ReleaseResource { resource: 73 },
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
    fn window_frame_flags_and_epoch_round_trip_exactly() {
        let frame = LocalWindowFrame {
            pane: 7,
            resource: 11,
            source: [0, 0, 300, 180],
            active: true,
            resize_epoch: Some(41),
        };
        let Ok(record) = encode_window_frame_record(FRAME_WINDOW_DMABUF, 9, 13, frame) else {
            return;
        };
        assert_eq!(record.flags, 1);
        assert_eq!(record.arguments[4], 41);
        assert_eq!(decode_window_frame_record(&record), Ok(frame));

        let mut reserved_flag = record;
        reserved_flag.flags |= 2;
        assert_eq!(
            decode_window_frame_record(&reserved_flag),
            Err(LocalProtocolError::InvalidValue)
        );
    }

    #[test]
    fn managed_resize_status_round_trips_and_rejects_malformed_values() {
        let configured = ManagedResizeStatus {
            pane: 7,
            resize_epoch: 41,
            result: ManagedResizeStatusResult::Configured {
                logical_size: [200, 120],
                pixel_size: [300, 180],
                scale_120: 180,
            },
        };
        let Ok(configured_record) = encode_managed_resize_status_record(configured, 9, 13) else {
            return;
        };
        assert_eq!(
            decode_managed_resize_status_record(&configured_record),
            Ok(configured)
        );

        for reason in [
            ManagedResizeRejectionReason::PaneUnavailable,
            ManagedResizeRejectionReason::CorrelationCapacity,
            ManagedResizeRejectionReason::ConfigureFailed,
            ManagedResizeRejectionReason::SizeUnsatisfied,
        ] {
            let rejected = ManagedResizeStatus {
                pane: 7,
                resize_epoch: 41,
                result: ManagedResizeStatusResult::Rejected { reason },
            };
            let Ok(record) = encode_managed_resize_status_record(rejected, 9, 13) else {
                return;
            };
            assert_eq!(decode_managed_resize_status_record(&record), Ok(rejected));
        }

        let mut malformed = configured_record;
        malformed.arguments[4] = 0;
        assert_eq!(
            decode_managed_resize_status_record(&malformed),
            Err(LocalProtocolError::InvalidValue)
        );
        let wrong_opcode = CriticalRecord::default();
        assert_eq!(
            decode_managed_resize_status_record(&wrong_opcode),
            Err(LocalProtocolError::InvalidValue)
        );
    }

    #[test]
    fn local_resize_input_rejects_zero_units_and_present_zero_epoch() {
        let invalid = ClientMessage::Input(InputEvent::WindowResize {
            pane: 7,
            logical_width: 200,
            logical_height: 120,
            pixel_width: 300,
            pixel_height: 180,
            scale_120: 180,
            resize_epoch: Some(0),
        });
        assert_eq!(
            encode_client_record(invalid, 9, 13),
            Err(LocalProtocolError::InvalidValue)
        );
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
