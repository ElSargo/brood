//! Versioned facet wire codec with allocation admission before decoding lists.
//!
//! Callers retain ownership of the decoded allocation budget for the lifetime
//! of the result. These limits bound one decode; they are not a global budget.

use crate::facets::{
    HostRequest, InputEvent, InputRequest, InputResponse, InputToken, InvocationLimits,
    PaneContent, PaneOutput, PaneTarget, RenderRequest, RenderResponse, RenderToken, Scope,
};

#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_wire_bytes: usize,
    pub max_allocated_bytes: usize,
    pub max_allocations: usize,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CodecError {
    #[error("facet message exceeds its admitted limits")]
    Limit,
    #[error("malformed or unsupported facet message")]
    Invalid,
}

struct Reader<'a> {
    bytes: &'a [u8],
    allocation_bytes: usize,
    allocations: usize,
    limits: DecodeLimits,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], CodecError> {
        let (head, tail) = self
            .bytes
            .split_at_checked(count)
            .ok_or(CodecError::Invalid)?;
        self.bytes = tail;
        Ok(head)
    }

    fn admit(&mut self, bytes: usize) -> Result<(), CodecError> {
        if bytes == 0 {
            return Ok(());
        }
        self.allocation_bytes = self
            .allocation_bytes
            .checked_add(bytes)
            .ok_or(CodecError::Limit)?;
        self.allocations = self.allocations.checked_add(1).ok_or(CodecError::Limit)?;
        if self.allocation_bytes > self.limits.max_allocated_bytes
            || self.allocations > self.limits.max_allocations
        {
            return Err(CodecError::Limit);
        }
        Ok(())
    }
}

struct Writer<'a> {
    bytes: Option<&'a mut [u8]>,
    written: usize,
}
impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        let end = self
            .written
            .checked_add(bytes.len())
            .ok_or(CodecError::Limit)?;
        if let Some(output) = self.bytes.as_mut() {
            output
                .get_mut(self.written..end)
                .ok_or(CodecError::Limit)?
                .copy_from_slice(bytes);
        }
        self.written = end;
        Ok(())
    }
}

trait Wire: Sized {
    const MIN_BYTES: usize;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError>;
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError>;
    fn read_values(reader: &mut Reader<'_>, count: usize) -> Result<Vec<Self>, CodecError> {
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(Self::read(reader)?);
        }
        Ok(values)
    }
    fn write_values(values: &[Self], writer: &mut Writer<'_>) -> Result<(), CodecError> {
        for value in values {
            value.write(writer)?;
        }
        Ok(())
    }
}

macro_rules! number {
    ($ty:ty, $size:expr) => {
        impl Wire for $ty {
            const MIN_BYTES: usize = $size;
            fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
                Ok(Self::from_le_bytes(
                    reader
                        .take($size)?
                        .try_into()
                        .map_err(|_| CodecError::Invalid)?,
                ))
            }
            fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
                writer.put(&self.to_le_bytes())
            }
        }
    };
}
impl Wire for u8 {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        reader.take(1)?.first().copied().ok_or(CodecError::Invalid)
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        writer.put(&[*self])
    }
    fn read_values(reader: &mut Reader<'_>, count: usize) -> Result<Vec<Self>, CodecError> {
        // Vec admission has already reserved the complete payload allocation.
        Ok(reader.take(count)?.to_vec())
    }
    fn write_values(values: &[Self], writer: &mut Writer<'_>) -> Result<(), CodecError> {
        writer.put(values)
    }
}
number!(u32, 4);
number!(u64, 8);
number!(f64, 8);

impl Wire for bool {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match u8::read(reader)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid),
        }
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        u8::from(*self).write(writer)
    }
}

impl<T: Wire> Wire for Vec<T> {
    const MIN_BYTES: usize = 4;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let count =
            usize::try_from(u32::read(reader)?).map_err(|_conversion_error| CodecError::Limit)?;
        let minimum = count.checked_mul(T::MIN_BYTES).ok_or(CodecError::Limit)?;
        if minimum > reader.bytes.len() {
            return Err(CodecError::Invalid);
        }
        reader.admit(
            count
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(CodecError::Limit)?,
        )?;
        T::read_values(reader, count)
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        u32::try_from(self.len())
            .map_err(|_conversion_error| CodecError::Limit)?
            .write(writer)?;
        T::write_values(self, writer)
    }
}

impl Wire for String {
    const MIN_BYTES: usize = 4;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Self::from_utf8(Vec::<u8>::read(reader)?).map_err(|_utf8_error| CodecError::Invalid)
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        u32::try_from(self.len())
            .map_err(|_conversion_error| CodecError::Limit)?
            .write(writer)?;
        writer.put(self.as_bytes())
    }
}

impl<T: Wire> Wire for Option<T> {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        if bool::read(reader)? {
            Ok(Some(T::read(reader)?))
        } else {
            Ok(None)
        }
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        self.is_some().write(writer)?;
        if let Some(value) = self {
            value.write(writer)?;
        }
        Ok(())
    }
}

impl Wire for [f64; 4] {
    const MIN_BYTES: usize = 32;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok([
            f64::read(reader)?,
            f64::read(reader)?,
            f64::read(reader)?,
            f64::read(reader)?,
        ])
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        for value in self {
            value.write(writer)?;
        }
        Ok(())
    }
}

macro_rules! record {
    ($ty:ident { $($field:ident: $field_ty:ty),* $(,)? }) => {
        impl Wire for $ty {
            const MIN_BYTES: usize = 0 $(+ <$field_ty as Wire>::MIN_BYTES)*;
            fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
                Ok(Self { $($field: <$field_ty>::read(reader)?),* })
            }
            fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
                $(self.$field.write(writer)?;)*
                Ok(())
            }
        }
    };
}

record!(Scope {
    client_generation: u64,
    actor_generation: u64,
    facet_generation: u64
});
record!(RenderToken {
    scope: Scope,
    invocation: u64,
    snapshot_revision: u64,
    candidate: u64,
    pane_membership_revision: u64,
    geometry_revision: u64,
    host_configuration_revision: u64,
    renderer_generation: u64,
    interaction_revision: u64
});
record!(PaneTarget {
    pane: u64,
    logical_width: f64,
    logical_height: f64,
    physical_width: u32,
    physical_height: u32,
    scale: f64,
    clip: [f64; 4],
    visible: bool,
    focused: bool
});
record!(RenderRequest { token:RenderToken, panes:Vec<PaneTarget>, target_time_ns:u64, cutoff_ns:u64, configuration:Vec<u8>, application_view:Vec<u8>, sequence_state:Vec<u8>, limits:InvocationLimits });
record!(InvocationLimits {
    max_output_bytes: u32,
    max_packet_bytes: u32,
    max_interaction_bytes: u32,
    max_sequence_bytes: u32,
    max_events: u32,
    max_event_bytes: u32,
    max_host_requests: u32,
    max_resources: u32,
    max_resource_bytes: u64
});
record!(RenderResponse { token:RenderToken, panes:Vec<PaneOutput>, next_frame_ns:Option<u64> });
record!(PaneOutput {
    pane: u64,
    content: PaneContent
});
record!(InputToken {
    scope: Scope,
    input_sequence: u64,
    frame: u64,
    pane: u64,
    snapshot_revision: u64,
    sequence_revision: u64
});
record!(InputRequest { token:InputToken, geometry:PaneTarget, event:InputEvent, interaction:Vec<u8>, sequence_state:Vec<u8>, limits:InvocationLimits });
record!(InputResponse { token:InputToken, next_sequence_state:Vec<u8>, events:Vec<Vec<u8>>, requests:Vec<HostRequest> });

impl Wire for PaneContent {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match u8::read(reader)? {
            0 => Ok(Self::Draw {
                packet: Vec::read(reader)?,
                interaction: Vec::read(reader)?,
            }),
            1 => Ok(Self::NoChange),
            _ => Err(CodecError::Invalid),
        }
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            Self::Draw {
                packet,
                interaction,
            } => {
                0_u8.write(writer)?;
                packet.write(writer)?;
                interaction.write(writer)
            }
            Self::NoChange => 1_u8.write(writer),
        }
    }
}

impl Wire for HostRequest {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match u8::read(reader)? {
            0 => Ok(Self::Capture),
            1 => Ok(Self::ReleaseCapture),
            2 => Ok(Self::Focus),
            3 => Ok(Self::Render),
            _ => Err(CodecError::Invalid),
        }
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            Self::Capture => 0_u8,
            Self::ReleaseCapture => 1,
            Self::Focus => 2,
            Self::Render => 3,
        }
        .write(writer)
    }
}

fn encode<T: Wire>(value: &T, kind: u8, output: &mut [u8]) -> Result<usize, CodecError> {
    let mut writer = Writer {
        bytes: Some(output),
        written: 0,
    };
    writer.put(b"BFC\x01")?;
    kind.write(&mut writer)?;
    value.write(&mut writer)?;
    Ok(writer.written)
}

fn decode<T: Wire>(bytes: &[u8], kind: u8, limits: DecodeLimits) -> Result<T, CodecError> {
    if bytes.len() > limits.max_wire_bytes {
        return Err(CodecError::Limit);
    }
    let mut reader = Reader {
        bytes,
        allocation_bytes: 0,
        allocations: 0,
        limits,
    };
    if reader.take(4)? != b"BFC\x01" || u8::read(&mut reader)? != kind {
        return Err(CodecError::Invalid);
    }
    let value = T::read(&mut reader)?;
    if !reader.bytes.is_empty() {
        return Err(CodecError::Invalid);
    }
    Ok(value)
}

macro_rules! entry {
    ($encode:ident, $decode:ident, $ty:ty, $kind:expr) => {
        pub fn $encode(value: &$ty, output: &mut [u8]) -> Result<usize, CodecError> {
            encode(value, $kind, output)
        }
        pub fn $decode(bytes: &[u8], limits: DecodeLimits) -> Result<$ty, CodecError> {
            decode(bytes, $kind, limits)
        }
    };
}
entry!(
    encode_render_request,
    decode_render_request,
    RenderRequest,
    0
);
entry!(
    encode_render_response,
    decode_render_response,
    RenderResponse,
    1
);
entry!(encode_input_request, decode_input_request, InputRequest, 2);
entry!(
    encode_input_response,
    decode_input_response,
    InputResponse,
    3
);

impl Wire for InputEvent {
    const MIN_BYTES: usize = 1;
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match u8::read(reader)? {
            0 => Ok(Self::PointerMotion {
                x: f64::read(reader)?,
                y: f64::read(reader)?,
            }),
            1 => Ok(Self::PointerButton {
                x: f64::read(reader)?,
                y: f64::read(reader)?,
                button: u32::read(reader)?,
                pressed: bool::read(reader)?,
                modifiers: u32::read(reader)?,
            }),
            2 => Ok(Self::PointerScroll {
                x: f64::read(reader)?,
                y: f64::read(reader)?,
                horizontal: f64::read(reader)?,
                vertical: f64::read(reader)?,
                modifiers: u32::read(reader)?,
            }),
            3 => Ok(Self::Key {
                code: u32::read(reader)?,
                logical_key: String::read(reader)?,
                modifiers: u32::read(reader)?,
                pressed: bool::read(reader)?,
                repeat: bool::read(reader)?,
            }),
            4 => Ok(Self::Text {
                text: String::read(reader)?,
            }),
            5 => Ok(Self::Paste {
                text: String::read(reader)?,
            }),
            6 => Ok(Self::Image {
                name: String::read(reader)?,
                mime_type: String::read(reader)?,
                bytes: Vec::<u8>::read(reader)?,
                preview_width: u32::read(reader)?,
                preview_height: u32::read(reader)?,
                preview_rgba8: Vec::<u8>::read(reader)?,
            }),
            7 => Ok(Self::Composition {
                text: String::read(reader)?,
                committed: bool::read(reader)?,
            }),
            8 => Ok(Self::FocusLost),
            9 => Ok(Self::CaptureLost),
            10 => Ok(Self::PointerLeave),
            _ => Err(CodecError::Invalid),
        }
    }
    fn write(&self, writer: &mut Writer<'_>) -> Result<(), CodecError> {
        match self {
            Self::PointerMotion { x, y } => {
                0_u8.write(writer)?;
                x.write(writer)?;
                y.write(writer)?;
                Ok(())
            }
            Self::PointerButton {
                x,
                y,
                button,
                pressed,
                modifiers,
            } => {
                1_u8.write(writer)?;
                x.write(writer)?;
                y.write(writer)?;
                button.write(writer)?;
                pressed.write(writer)?;
                modifiers.write(writer)?;
                Ok(())
            }
            Self::PointerScroll {
                x,
                y,
                horizontal,
                vertical,
                modifiers,
            } => {
                2_u8.write(writer)?;
                x.write(writer)?;
                y.write(writer)?;
                horizontal.write(writer)?;
                vertical.write(writer)?;
                modifiers.write(writer)?;
                Ok(())
            }
            Self::Key {
                code,
                logical_key,
                modifiers,
                pressed,
                repeat,
            } => {
                3_u8.write(writer)?;
                code.write(writer)?;
                logical_key.write(writer)?;
                modifiers.write(writer)?;
                pressed.write(writer)?;
                repeat.write(writer)?;
                Ok(())
            }
            Self::Text { text } => {
                4_u8.write(writer)?;
                text.write(writer)?;
                Ok(())
            }
            Self::Paste { text } => {
                5_u8.write(writer)?;
                text.write(writer)?;
                Ok(())
            }
            Self::Image {
                name,
                mime_type,
                bytes,
                preview_width,
                preview_height,
                preview_rgba8,
            } => {
                6_u8.write(writer)?;
                name.write(writer)?;
                mime_type.write(writer)?;
                bytes.write(writer)?;
                preview_width.write(writer)?;
                preview_height.write(writer)?;
                preview_rgba8.write(writer)?;
                Ok(())
            }
            Self::Composition { text, committed } => {
                7_u8.write(writer)?;
                text.write(writer)?;
                committed.write(writer)?;
                Ok(())
            }
            Self::FocusLost => {
                8_u8.write(writer)?;
                Ok(())
            }
            Self::CaptureLost => {
                9_u8.write(writer)?;
                Ok(())
            }
            Self::PointerLeave => {
                10_u8.write(writer)?;
                Ok(())
            }
        }
    }
}

/// Encode into an exactly sized allocation after checking the negotiated ceiling.
/// The sizing pass traverses borrowed data without allocating or copying payloads.
pub fn encode_input_response_owned(
    value: &InputResponse,
    max_bytes: usize,
) -> Result<Vec<u8>, CodecError> {
    encode_owned(value, 3, max_bytes)
}

pub fn encode_render_response_owned(
    value: &RenderResponse,
    max_bytes: usize,
) -> Result<Vec<u8>, CodecError> {
    encode_owned(value, 1, max_bytes)
}

pub fn encode_render_request_owned(
    value: &RenderRequest,
    max_bytes: usize,
) -> Result<Vec<u8>, CodecError> {
    encode_owned(value, 0, max_bytes)
}

pub fn encode_input_request_owned(
    value: &InputRequest,
    max_bytes: usize,
) -> Result<Vec<u8>, CodecError> {
    encode_owned(value, 2, max_bytes)
}

fn encode_owned<T: Wire>(value: &T, kind: u8, max_bytes: usize) -> Result<Vec<u8>, CodecError> {
    let mut sizing = Writer {
        bytes: None,
        written: 5,
    };
    value.write(&mut sizing)?;
    if sizing.written > max_bytes {
        return Err(CodecError::Limit);
    }
    let mut bytes = vec![0; sizing.written];
    encode(value, kind, &mut bytes)?;
    Ok(bytes)
}
