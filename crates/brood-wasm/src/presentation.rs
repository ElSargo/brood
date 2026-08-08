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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontIntent {
    Ui,
    Monospace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontWeight {
    Normal,
    Bold,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontStyle {
    Normal,
    Italic,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayoutOptions {
    pub font_intent: FontIntent,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
    pub font_size: f32,
    pub max_width: f32,
    pub line_height: f32,
    /// Straight-alpha sRGB red, green, blue, and alpha channels.
    pub color: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextLayoutResult {
    pub handle: u64,
    pub width: f32,
    pub height: f32,
    pub first_baseline: f32,
    pub line_count: u32,
    /// Stable advance of one cell for monospace intent, otherwise zero.
    pub cell_width: f32,
    /// Resolved line height shared by measurement and rendering.
    pub line_height: f32,
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
    PointerLeft {
        pane: u64,
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
        logical_key: String,
        pressed: bool,
        repeat: bool,
        modifiers: u32,
    },
    Text {
        pane: u64,
        text: String,
    },
    Paste {
        pane: u64,
        text: String,
    },
    Image {
        pane: u64,
        name: String,
        mime_type: String,
        bytes: Vec<u8>,
        preview_width: u32,
        preview_height: u32,
        preview_rgba8: Vec<u8>,
    },
}

/// One backend surface session as observed by a presentation-local actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    pub id: u64,
    pub name: String,
    pub active: bool,
    pub windows: u64,
    pub working_directory: Option<String>,
    pub project_id: String,
    pub project_name: String,
    pub project_directory: String,
    pub branch: Option<String>,
    pub worktree: bool,
    pub git: bool,
}

/// Latest-state session catalogue delivered by the presentation client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub backend_id: u64,
    pub revision: u64,
    pub active: u64,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionOperation {
    Refresh,
    Activate,
    Create,
    Rename,
    Close,
}

/// One actor-owned request to mutate or refresh the session catalogue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCommandRequest {
    pub backend_id: u64,
    pub request: u64,
    pub operation: SessionOperation,
    pub session: Option<u64>,
    pub name: Option<String>,
    /// Optional directory used by backend-qualified session creation.
    pub directory: Option<String>,
}

/// Terminal result for a [`SessionCommandRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCommandCompletion {
    pub backend_id: u64,
    pub request: u64,
    pub operation: SessionOperation,
    pub success: bool,
    pub session: Option<u64>,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Snapshot(SessionSnapshot),
    CommandCompleted(SessionCommandCompletion),
}
