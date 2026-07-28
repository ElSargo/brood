//! Brood's bounded, renderer-independent canvas packet codec.

use std::{error::Error, fmt};

pub mod theme;

pub const PACKET_VERSION: u16 = 1;
pub const MAX_PACKET_BYTES: usize = 1024 * 1024;
pub const MAX_COMMANDS: usize = 16_384;
pub const MAX_PATH_SEGMENTS: usize = 65_536;
pub const MAX_SAVED_STATES: usize = 32;
pub const MAX_LAYER_DEPTH: usize = 8;
pub const MAX_CLIP_DEPTH: usize = 32;
pub const MAX_ABSOLUTE_COORDINATE: f32 = 1_000_000.0;
pub const MAX_TRANSFORM_COMPONENT: f32 = 1_000_000.0;

const MAGIC: [u8; 4] = *b"MFGX";
const HEADER_BYTES: usize = 16;

/// A straight-alpha sRGB color. The renderer converts it to linear premultiplied
/// form before compositing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    #[must_use]
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    fn validate(self) -> Result<(), PacketError> {
        if [self.r, self.g, self.b, self.a]
            .into_iter()
            .all(|channel| channel.is_finite() && (0.0..=1.0).contains(&channel))
        {
            Ok(())
        } else {
            Err(PacketError::Invalid(
                "color channels must be finite and in 0..=1",
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn validate(self) -> Result<(), PacketError> {
        let values = [self.x, self.y, self.width, self.height];
        if !values.into_iter().all(f32::is_finite) || self.width < 0.0 || self.height < 0.0 {
            return Err(PacketError::Invalid(
                "rectangle values must be finite and extents non-negative",
            ));
        }
        validate_coordinate_magnitude(&values)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub sx: f32,
    pub ky: f32,
    pub kx: f32,
    pub sy: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        sx: 1.0,
        ky: 0.0,
        kx: 0.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    fn validate(self) -> Result<(), PacketError> {
        let values = [self.sx, self.ky, self.kx, self.sy, self.tx, self.ty];
        if !values.into_iter().all(f32::is_finite) {
            return Err(PacketError::Invalid("transform values must be finite"));
        }
        if values
            .into_iter()
            .any(|value| value.abs() > MAX_TRANSFORM_COMPONENT)
        {
            return Err(PacketError::Limit("transform magnitude limit exceeded"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BlendMode {
    SourceOver = 0,
    Multiply = 1,
    Screen = 2,
    Overlay = 3,
    Darken = 4,
    Lighten = 5,
    Difference = 6,
    Plus = 7,
}

impl BlendMode {
    const fn decode(value: u8) -> Result<Self, PacketError> {
        match value {
            0 => Ok(Self::SourceOver),
            1 => Ok(Self::Multiply),
            2 => Ok(Self::Screen),
            3 => Ok(Self::Overlay),
            4 => Ok(Self::Darken),
            5 => Ok(Self::Lighten),
            6 => Ok(Self::Difference),
            7 => Ok(Self::Plus),
            _ => Err(PacketError::Invalid("unknown blend mode")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathSegment {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub segments: Vec<PathSegment>,
}

impl Path {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.segments.push(PathSegment::MoveTo(x, y));
        self
    }

    #[must_use]
    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.segments.push(PathSegment::LineTo(x, y));
        self
    }

    #[must_use]
    pub fn quad_to(mut self, x1: f32, y1: f32, x: f32, y: f32) -> Self {
        self.segments.push(PathSegment::QuadTo(x1, y1, x, y));
        self
    }

    #[must_use]
    pub fn cubic_to(mut self, first: (f32, f32), second: (f32, f32), end: (f32, f32)) -> Self {
        self.segments.push(PathSegment::CubicTo(
            first.0, first.1, second.0, second.1, end.0, end.1,
        ));
        self
    }

    #[must_use]
    pub fn close(mut self) -> Self {
        self.segments.push(PathSegment::Close);
        self
    }

    fn validate(&self) -> Result<(), PacketError> {
        if self.segments.is_empty() {
            return Err(PacketError::Invalid(
                "paths must contain at least one segment",
            ));
        }
        if self.segments.len() > MAX_PATH_SEGMENTS {
            return Err(PacketError::Limit("path segment limit exceeded"));
        }
        let finite = |values: &[f32]| values.iter().all(|value| value.is_finite());
        for segment in &self.segments {
            let valid = match segment {
                PathSegment::MoveTo(x, y) | PathSegment::LineTo(x, y) => finite(&[*x, *y]),
                PathSegment::QuadTo(x1, y1, x, y) => finite(&[*x1, *y1, *x, *y]),
                PathSegment::CubicTo(x1, y1, x2, y2, x, y) => finite(&[*x1, *y1, *x2, *y2, *x, *y]),
                PathSegment::Close => true,
            };
            if !valid {
                return Err(PacketError::Invalid("path coordinates must be finite"));
            }
            let bounded = match segment {
                PathSegment::MoveTo(x, y) | PathSegment::LineTo(x, y) => [*x, *y]
                    .into_iter()
                    .all(|value| value.abs() <= MAX_ABSOLUTE_COORDINATE),
                PathSegment::QuadTo(x1, y1, x, y) => [*x1, *y1, *x, *y]
                    .into_iter()
                    .all(|value| value.abs() <= MAX_ABSOLUTE_COORDINATE),
                PathSegment::CubicTo(x1, y1, x2, y2, x, y) => [*x1, *y1, *x2, *y2, *x, *y]
                    .into_iter()
                    .all(|value| value.abs() <= MAX_ABSOLUTE_COORDINATE),
                PathSegment::Close => true,
            };
            if !bounded {
                return Err(PacketError::Limit("coordinate magnitude limit exceeded"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Save,
    Restore,
    SetTransform(Transform),
    SetOpacity(f32),
    SetBlendMode(BlendMode),
    ClipRect(Rect),
    ClipPath(Path),
    FillRect {
        rect: Rect,
        color: Color,
    },
    StrokeRect {
        rect: Rect,
        color: Color,
        width: f32,
    },
    FillRoundedRect {
        rect: Rect,
        radius: f32,
        color: Color,
    },
    StrokeRoundedRect {
        rect: Rect,
        radius: f32,
        color: Color,
        width: f32,
    },
    FillPath {
        path: Path,
        color: Color,
    },
    StrokePath {
        path: Path,
        color: Color,
        width: f32,
    },
    DrawImage {
        image: u64,
        destination: Rect,
    },
    DrawTextLayout {
        layout: u64,
        x: f32,
        y: f32,
    },
    BeginLayer,
    EndLayer,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Packet {
    pub commands: Vec<Command>,
}

impl Packet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, command: Command) {
        self.commands.push(command);
    }

    pub fn encode(&self) -> Result<Vec<u8>, PacketError> {
        self.validate()?;
        let mut bytes = Vec::with_capacity(HEADER_BYTES + self.commands.len() * 32);
        bytes.extend_from_slice(&MAGIC);
        put_u16(&mut bytes, PACKET_VERSION);
        put_u16(&mut bytes, 0);
        put_u32(
            &mut bytes,
            u32::try_from(self.commands.len())
                .map_err(|_overflow| PacketError::Limit("command limit exceeded"))?,
        );
        put_u32(&mut bytes, 0);
        for command in &self.commands {
            bytes.push(command_opcode(command));
            bytes.extend_from_slice(&[0, 0, 0]);
            let length_offset = bytes.len();
            put_u32(&mut bytes, 0);
            let payload_offset = bytes.len();
            encode_command(command, &mut bytes);
            let payload_len = u32::try_from(bytes.len() - payload_offset)
                .map_err(|_overflow| PacketError::Limit("command payload limit exceeded"))?;
            bytes
                .get_mut(length_offset..length_offset + 4)
                .ok_or(PacketError::Invalid("command length field disappeared"))?
                .copy_from_slice(&payload_len.to_le_bytes());
        }
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(PacketError::Limit("packet byte limit exceeded"));
        }
        let total = u32::try_from(bytes.len())
            .map_err(|_overflow| PacketError::Limit("packet byte limit exceeded"))?;
        bytes
            .get_mut(12..16)
            .ok_or(PacketError::Invalid("packet length field disappeared"))?
            .copy_from_slice(&total.to_le_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PacketError> {
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(PacketError::Limit("packet byte limit exceeded"));
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != MAGIC {
            return Err(PacketError::Invalid("invalid packet magic"));
        }
        if reader.u16()? != PACKET_VERSION {
            return Err(PacketError::Invalid("unsupported packet version"));
        }
        if reader.u16()? != 0 {
            return Err(PacketError::Invalid("packet reserved field must be zero"));
        }
        let command_count = usize::try_from(reader.u32()?)
            .map_err(|_overflow| PacketError::Limit("command limit exceeded"))?;
        if command_count > MAX_COMMANDS {
            return Err(PacketError::Limit("command limit exceeded"));
        }
        let declared_bytes = usize::try_from(reader.u32()?)
            .map_err(|_overflow| PacketError::Limit("packet byte limit exceeded"))?;
        if declared_bytes != bytes.len() {
            return Err(PacketError::Invalid(
                "packet byte length does not match header",
            ));
        }
        let mut commands = Vec::with_capacity(command_count);
        for _ in 0..command_count {
            let opcode = reader.u8()?;
            if reader.take(3)? != [0, 0, 0] {
                return Err(PacketError::Invalid("command reserved field must be zero"));
            }
            let payload_len = usize::try_from(reader.u32()?)
                .map_err(|_overflow| PacketError::Invalid("command payload length overflow"))?;
            commands.push(decode_command(opcode, reader.take(payload_len)?)?);
        }
        if !reader.is_empty() {
            return Err(PacketError::Invalid("packet contains trailing bytes"));
        }
        let packet = Self { commands };
        packet.validate()?;
        Ok(packet)
    }

    pub fn validate(&self) -> Result<(), PacketError> {
        if self.commands.len() > MAX_COMMANDS {
            return Err(PacketError::Limit("command limit exceeded"));
        }
        let mut saved_clips = Vec::new();
        let mut clips = 0_usize;
        let mut layers = 0_usize;
        for command in &self.commands {
            validate_command(command)?;
            match command {
                Command::Save => {
                    if saved_clips.len() >= MAX_SAVED_STATES {
                        return Err(PacketError::Limit("saved-state depth limit exceeded"));
                    }
                    saved_clips.push(clips);
                }
                Command::Restore => {
                    clips = saved_clips
                        .pop()
                        .ok_or(PacketError::Invalid("restore without matching save"))?;
                }
                Command::ClipRect(_) | Command::ClipPath(_) => {
                    clips = clips.saturating_add(1);
                    if clips > MAX_CLIP_DEPTH {
                        return Err(PacketError::Limit("clip depth limit exceeded"));
                    }
                }
                Command::BeginLayer => {
                    layers = layers.saturating_add(1);
                    if layers > MAX_LAYER_DEPTH {
                        return Err(PacketError::Limit("layer depth limit exceeded"));
                    }
                }
                Command::EndLayer => {
                    layers = layers.checked_sub(1).ok_or(PacketError::Invalid(
                        "end-layer without matching begin-layer",
                    ))?;
                }
                _ => {}
            }
        }
        if !saved_clips.is_empty() {
            return Err(PacketError::Invalid("unbalanced saved-state stack"));
        }
        if layers != 0 {
            return Err(PacketError::Invalid("unbalanced layer stack"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketError {
    Invalid(&'static str),
    Limit(&'static str),
}

impl fmt::Display for PacketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid draw packet: {message}"),
            Self::Limit(message) => write!(formatter, "draw packet limit: {message}"),
        }
    }
}

impl Error for PacketError {}

fn validate_command(command: &Command) -> Result<(), PacketError> {
    match command {
        Command::Save
        | Command::Restore
        | Command::SetBlendMode(_)
        | Command::BeginLayer
        | Command::EndLayer => Ok(()),
        Command::SetTransform(transform) => transform.validate(),
        Command::SetOpacity(opacity) => validate_unit(*opacity, "opacity must be in 0..=1"),
        Command::ClipRect(rect) => rect.validate(),
        Command::ClipPath(path) => path.validate(),
        Command::FillRect { rect, color } => {
            rect.validate()?;
            color.validate()
        }
        Command::StrokeRect { rect, color, width } => {
            rect.validate()?;
            color.validate()?;
            validate_positive(*width, "stroke width must be positive")
        }
        Command::FillRoundedRect {
            rect,
            radius,
            color,
        } => {
            rect.validate()?;
            validate_non_negative(*radius, "corner radius must be non-negative")?;
            color.validate()
        }
        Command::StrokeRoundedRect {
            rect,
            radius,
            color,
            width,
        } => {
            rect.validate()?;
            validate_non_negative(*radius, "corner radius must be non-negative")?;
            color.validate()?;
            validate_positive(*width, "stroke width must be positive")
        }
        Command::FillPath { path, color } => {
            path.validate()?;
            color.validate()
        }
        Command::StrokePath { path, color, width } => {
            path.validate()?;
            color.validate()?;
            validate_positive(*width, "stroke width must be positive")
        }
        Command::DrawImage { image, destination } => {
            if *image == 0 {
                return Err(PacketError::Invalid("image handle must be non-zero"));
            }
            destination.validate()
        }
        Command::DrawTextLayout { layout, x, y } => {
            if *layout == 0 {
                return Err(PacketError::Invalid("text layout handle must be non-zero"));
            }
            if !x.is_finite() || !y.is_finite() {
                return Err(PacketError::Invalid("text position must be finite"));
            }
            validate_coordinate_magnitude(&[*x, *y])
        }
    }
}

const fn command_opcode(command: &Command) -> u8 {
    match command {
        Command::Save => 1,
        Command::Restore => 2,
        Command::SetTransform(_) => 3,
        Command::SetOpacity(_) => 4,
        Command::SetBlendMode(_) => 5,
        Command::ClipRect(_) => 6,
        Command::ClipPath(_) => 7,
        Command::FillRect { .. } => 8,
        Command::StrokeRect { .. } => 9,
        Command::FillRoundedRect { .. } => 10,
        Command::StrokeRoundedRect { .. } => 11,
        Command::FillPath { .. } => 12,
        Command::StrokePath { .. } => 13,
        Command::DrawImage { .. } => 14,
        Command::DrawTextLayout { .. } => 15,
        Command::BeginLayer => 16,
        Command::EndLayer => 17,
    }
}

fn encode_command(command: &Command, bytes: &mut Vec<u8>) {
    match command {
        Command::Save | Command::Restore | Command::BeginLayer | Command::EndLayer => {}
        Command::SetTransform(transform) => put_transform(bytes, *transform),
        Command::SetOpacity(opacity) => put_f32(bytes, *opacity),
        Command::SetBlendMode(mode) => bytes.push(*mode as u8),
        Command::ClipRect(rect) => put_rect(bytes, *rect),
        Command::ClipPath(path) => put_path(bytes, path),
        Command::FillRect { rect, color } => {
            put_rect(bytes, *rect);
            put_color(bytes, *color);
        }
        Command::StrokeRect { rect, color, width } => {
            put_rect(bytes, *rect);
            put_color(bytes, *color);
            put_f32(bytes, *width);
        }
        Command::FillRoundedRect {
            rect,
            radius,
            color,
        } => {
            put_rect(bytes, *rect);
            put_f32(bytes, *radius);
            put_color(bytes, *color);
        }
        Command::StrokeRoundedRect {
            rect,
            radius,
            color,
            width,
        } => {
            put_rect(bytes, *rect);
            put_f32(bytes, *radius);
            put_color(bytes, *color);
            put_f32(bytes, *width);
        }
        Command::FillPath { path, color } => {
            put_path(bytes, path);
            put_color(bytes, *color);
        }
        Command::StrokePath { path, color, width } => {
            put_path(bytes, path);
            put_color(bytes, *color);
            put_f32(bytes, *width);
        }
        Command::DrawImage { image, destination } => {
            put_u64(bytes, *image);
            put_rect(bytes, *destination);
        }
        Command::DrawTextLayout { layout, x, y } => {
            put_u64(bytes, *layout);
            put_f32(bytes, *x);
            put_f32(bytes, *y);
        }
    }
}

fn decode_command(opcode: u8, payload: &[u8]) -> Result<Command, PacketError> {
    let mut reader = Reader::new(payload);
    let command = match opcode {
        1 => Command::Save,
        2 => Command::Restore,
        3 => Command::SetTransform(reader.transform()?),
        4 => Command::SetOpacity(reader.f32()?),
        5 => Command::SetBlendMode(BlendMode::decode(reader.u8()?)?),
        6 => Command::ClipRect(reader.rect()?),
        7 => Command::ClipPath(reader.path()?),
        8 => Command::FillRect {
            rect: reader.rect()?,
            color: reader.color()?,
        },
        9 => Command::StrokeRect {
            rect: reader.rect()?,
            color: reader.color()?,
            width: reader.f32()?,
        },
        10 => Command::FillRoundedRect {
            rect: reader.rect()?,
            radius: reader.f32()?,
            color: reader.color()?,
        },
        11 => Command::StrokeRoundedRect {
            rect: reader.rect()?,
            radius: reader.f32()?,
            color: reader.color()?,
            width: reader.f32()?,
        },
        12 => Command::FillPath {
            path: reader.path()?,
            color: reader.color()?,
        },
        13 => Command::StrokePath {
            path: reader.path()?,
            color: reader.color()?,
            width: reader.f32()?,
        },
        14 => Command::DrawImage {
            image: reader.u64()?,
            destination: reader.rect()?,
        },
        15 => Command::DrawTextLayout {
            layout: reader.u64()?,
            x: reader.f32()?,
            y: reader.f32()?,
        },
        16 => Command::BeginLayer,
        17 => Command::EndLayer,
        _ => return Err(PacketError::Invalid("unknown command opcode")),
    };
    if !reader.is_empty() {
        return Err(PacketError::Invalid("command contains trailing bytes"));
    }
    Ok(command)
}

fn put_path(bytes: &mut Vec<u8>, path: &Path) {
    put_u32(
        bytes,
        u32::try_from(path.segments.len()).unwrap_or(u32::MAX),
    );
    for segment in &path.segments {
        match segment {
            PathSegment::MoveTo(x, y) => {
                bytes.push(1);
                put_f32(bytes, *x);
                put_f32(bytes, *y);
            }
            PathSegment::LineTo(x, y) => {
                bytes.push(2);
                put_f32(bytes, *x);
                put_f32(bytes, *y);
            }
            PathSegment::QuadTo(x1, y1, x, y) => {
                bytes.push(3);
                for value in [*x1, *y1, *x, *y] {
                    put_f32(bytes, value);
                }
            }
            PathSegment::CubicTo(x1, y1, x2, y2, x, y) => {
                bytes.push(4);
                for value in [*x1, *y1, *x2, *y2, *x, *y] {
                    put_f32(bytes, value);
                }
            }
            PathSegment::Close => bytes.push(5),
        }
    }
}

fn put_rect(bytes: &mut Vec<u8>, rect: Rect) {
    for value in [rect.x, rect.y, rect.width, rect.height] {
        put_f32(bytes, value);
    }
}

fn put_color(bytes: &mut Vec<u8>, color: Color) {
    for value in [color.r, color.g, color.b, color.a] {
        put_f32(bytes, value);
    }
}

fn put_transform(bytes: &mut Vec<u8>, transform: Transform) {
    for value in [
        transform.sx,
        transform.ky,
        transform.kx,
        transform.sy,
        transform.tx,
        transform.ty,
    ] {
        put_f32(bytes, value);
    }
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_f32(bytes: &mut Vec<u8>, value: f32) {
    put_u32(bytes, value.to_bits());
}

fn validate_unit(value: f32, label: &'static str) -> Result<(), PacketError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(PacketError::Invalid(label))
    }
}

fn validate_non_negative(value: f32, label: &'static str) -> Result<(), PacketError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(PacketError::Invalid(label))
    }
}

fn validate_positive(value: f32, label: &'static str) -> Result<(), PacketError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(PacketError::Invalid(label));
    }
    if value > MAX_ABSOLUTE_COORDINATE {
        return Err(PacketError::Limit("stroke width limit exceeded"));
    }
    Ok(())
}

fn validate_coordinate_magnitude(values: &[f32]) -> Result<(), PacketError> {
    if values
        .iter()
        .any(|value| value.abs() > MAX_ABSOLUTE_COORDINATE)
    {
        Err(PacketError::Limit("coordinate magnitude limit exceeded"))
    } else {
        Ok(())
    }
}

struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], PacketError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(count)
            .ok_or(PacketError::Invalid("packet is truncated"))?;
        self.remaining = remaining;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, PacketError> {
        Ok(*self
            .take(1)?
            .first()
            .ok_or(PacketError::Invalid("packet is truncated"))?)
    }

    fn u16(&mut self) -> Result<u16, PacketError> {
        let mut bytes = [0_u8; 2];
        bytes.copy_from_slice(self.take(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, PacketError> {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, PacketError> {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn f32(&mut self) -> Result<f32, PacketError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn rect(&mut self) -> Result<Rect, PacketError> {
        Ok(Rect::new(
            self.f32()?,
            self.f32()?,
            self.f32()?,
            self.f32()?,
        ))
    }

    fn color(&mut self) -> Result<Color, PacketError> {
        Ok(Color::rgba(
            self.f32()?,
            self.f32()?,
            self.f32()?,
            self.f32()?,
        ))
    }

    fn transform(&mut self) -> Result<Transform, PacketError> {
        Ok(Transform {
            sx: self.f32()?,
            ky: self.f32()?,
            kx: self.f32()?,
            sy: self.f32()?,
            tx: self.f32()?,
            ty: self.f32()?,
        })
    }

    fn path(&mut self) -> Result<Path, PacketError> {
        let count = usize::try_from(self.u32()?)
            .map_err(|_overflow| PacketError::Limit("path segment limit exceeded"))?;
        if count > MAX_PATH_SEGMENTS {
            return Err(PacketError::Limit("path segment limit exceeded"));
        }
        let mut segments = Vec::with_capacity(count);
        for _ in 0..count {
            segments.push(match self.u8()? {
                1 => PathSegment::MoveTo(self.f32()?, self.f32()?),
                2 => PathSegment::LineTo(self.f32()?, self.f32()?),
                3 => PathSegment::QuadTo(self.f32()?, self.f32()?, self.f32()?, self.f32()?),
                4 => PathSegment::CubicTo(
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                    self.f32()?,
                ),
                5 => PathSegment::Close,
                _ => return Err(PacketError::Invalid("unknown path segment opcode")),
            });
        }
        Ok(Path { segments })
    }
}
