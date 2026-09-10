//! Protocol-neutral request and response shapes for host-invoked plugin facets.
//!
//! The byte fields in these records are opaque, plugin-owned values. This
//! module deliberately supplies serde data shapes only; transport codecs,
//! admission limits, and host validation beyond basic geometry are defined by
//! their owning runtime.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Scope {
    pub client_generation: u64,
    pub actor_generation: u64,
    pub facet_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenderToken {
    pub scope: Scope,
    pub invocation: u64,
    pub snapshot_revision: u64,
    pub candidate: u64,
    pub pane_membership_revision: u64,
    pub geometry_revision: u64,
    pub host_configuration_revision: u64,
    pub renderer_generation: u64,
    pub interaction_revision: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PaneTarget {
    pub pane: u64,
    pub logical_width: f64,
    pub logical_height: f64,
    pub physical_width: u32,
    pub physical_height: u32,
    pub scale: f64,
    /// x, y, width, height in logical pane coordinates.
    pub clip: [f64; 4],
    pub visible: bool,
    pub focused: bool,
}

impl PaneTarget {
    /// Validate the geometry values which must be safe to hand to a facet.
    ///
    /// This intentionally does not impose protocol or resource limits. Those
    /// limits belong to the host admission layer.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.logical_width.is_finite()
            || !self.logical_height.is_finite()
            || self.logical_width < 0.0
            || self.logical_height < 0.0
        {
            return Err("logical extent must be finite and non-negative");
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err("scale must be finite and positive");
        }
        if self.clip.iter().any(|value| !value.is_finite()) {
            return Err("clip must contain only finite values");
        }
        if self.clip[2] < 0.0 || self.clip[3] < 0.0 {
            return Err("clip extent must be non-negative");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RenderRequest {
    pub token: RenderToken,
    pub panes: Vec<PaneTarget>,
    pub target_time_ns: u64,
    pub cutoff_ns: u64,
    /// Encoded `brood_graphics::theme::ThemeSnapshot`; its revision equals
    /// `token.host_configuration_revision`. Immutable for the whole invocation.
    pub configuration: Vec<u8>,
    pub application_view: Vec<u8>,
    pub sequence_state: Vec<u8>,
    pub limits: InvocationLimits,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InvocationLimits {
    pub max_output_bytes: u32,
    pub max_packet_bytes: u32,
    pub max_interaction_bytes: u32,
    pub max_sequence_bytes: u32,
    pub max_events: u32,
    pub max_event_bytes: u32,
    pub max_host_requests: u32,
    pub max_resources: u32,
    pub max_resource_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenderResponse {
    pub token: RenderToken,
    pub panes: Vec<PaneOutput>,
    /// Desired later presentation opportunity. The host coalesces this demand
    /// onto its predicted output cell and pre-arms preparation; this is not a
    /// hard wall-clock not-before deadline.
    pub next_frame_ns: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaneOutput {
    pub pane: u64,
    pub content: PaneContent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PaneContent {
    Draw {
        packet: Vec<u8>,
        interaction: Vec<u8>,
    },
    NoChange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputToken {
    pub scope: Scope,
    pub input_sequence: u64,
    pub frame: u64,
    pub pane: u64,
    pub snapshot_revision: u64,
    pub sequence_revision: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InputRequest {
    pub token: InputToken,
    pub geometry: PaneTarget,
    pub event: InputEvent,
    pub interaction: Vec<u8>,
    pub sequence_state: Vec<u8>,
    pub limits: InvocationLimits,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum InputEvent {
    PointerMotion {
        x: f64,
        y: f64,
    },
    PointerButton {
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    PointerScroll {
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
        modifiers: u32,
    },
    Key {
        code: u32,
        logical_key: String,
        modifiers: u32,
        pressed: bool,
        repeat: bool,
    },
    Text {
        text: String,
    },
    Paste {
        text: String,
    },
    Image {
        name: String,
        mime_type: String,
        bytes: Vec<u8>,
        preview_width: u32,
        preview_height: u32,
        preview_rgba8: Vec<u8>,
    },
    Composition {
        text: String,
        committed: bool,
    },
    FocusLost,
    CaptureLost,
    PointerLeave,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputResponse {
    pub token: InputToken,
    pub next_sequence_state: Vec<u8>,
    pub events: Vec<Vec<u8>>,
    pub requests: Vec<HostRequest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum HostRequest {
    Capture,
    ReleaseCapture,
    Focus,
    Render,
}
