//! Presentation-local capabilities and typed events for component actors.

use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationError {
    Unavailable,
    InvalidPane,
    InvalidArgument,
    LimitReached,
    InvalidPacket,
    InvalidResource,
    ResourceInUse,
    HostFailure,
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "presentation service is unavailable",
            Self::InvalidPane => "presentation pane is invalid",
            Self::InvalidArgument => "presentation argument is invalid",
            Self::LimitReached => "presentation limit reached",
            Self::InvalidPacket => "draw packet is invalid",
            Self::InvalidResource => "graphics resource is invalid",
            Self::ResourceInUse => "graphics resource is still retained by a pane",
            Self::HostFailure => "presentation host failed",
        })
    }
}

impl Error for PresentationError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayoutOptions {
    pub font_size: f32,
    pub max_width: f32,
    pub line_height: f32,
    pub color: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayoutResult {
    pub handle: u64,
    pub width: f32,
    pub height: f32,
    pub first_baseline: f32,
    pub line_count: u32,
}

/// Client-supplied presentation capability installed into one actor Store.
pub trait PresentationHost: fmt::Debug + Send + Sync + 'static {
    fn submit_draw_packet(
        &self,
        actor_generation: u64,
        pane: u64,
        packet: Vec<u8>,
    ) -> Result<u64, PresentationError>;

    fn create_image_rgba8(
        &self,
        actor_generation: u64,
        pane: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Result<u64, PresentationError>;

    fn layout_text(
        &self,
        actor_generation: u64,
        pane: u64,
        text: String,
        options: TextLayoutOptions,
    ) -> Result<TextLayoutResult, PresentationError>;

    fn drop_resource(
        &self,
        actor_generation: u64,
        pane: u64,
        resource: u64,
    ) -> Result<(), PresentationError>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum PresentationEvent {
    PaneCreated {
        pane: u64,
        width: f32,
        height: f32,
        scale: f32,
    },
    PaneResized {
        pane: u64,
        width: f32,
        height: f32,
        scale: f32,
    },
    PaneFocused {
        pane: u64,
        focused: bool,
    },
    PointerMoved {
        pane: u64,
        x: f32,
        y: f32,
    },
    PointerButton {
        pane: u64,
        x: f32,
        y: f32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    PointerScroll {
        pane: u64,
        x: f32,
        y: f32,
        horizontal: f32,
        vertical: f32,
        modifiers: u32,
    },
    Key {
        pane: u64,
        physical_key: u32,
        pressed: bool,
        repeat: bool,
        modifiers: u32,
    },
    Text {
        pane: u64,
        text: String,
    },
}
