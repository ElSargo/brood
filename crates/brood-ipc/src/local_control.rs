//! Private local bootstrap, control, and descriptor-broker protocol.
//!
//! D-Bus is deliberately absent here. This packet protocol exists only to
//! establish the shared-memory transport and move generation-owned descriptors.

use brood_protocol::FrameTransportKind;
use serde::{Deserialize, Serialize};

/// Maximum encoded bootstrap request or response size.
pub const LOCAL_CONTROL_MAX_BYTES: usize = 64 * 1024;
/// Maximum descriptors in one bootstrap response.
pub const LOCAL_CONTROL_MAX_FDS: usize = 16;

/// One private request from a Brood-owned local endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalControlRequest {
    Handshake {
        protocol_version: u32,
    },
    Control {
        argv: Vec<String>,
        json: bool,
    },
    OpenPresentation {
        frame_transports: Vec<FrameTransportKind>,
    },
    DmabufResource {
        generation: u64,
        resource: u64,
    },
    SharedMemoryResource {
        generation: u64,
        resource: u64,
    },
    InstallSessionInitArtifact {
        path: String,
    },
    TakeRealmArtifactRequests,
    CompleteRealmArtifactRequest {
        request: u64,
        success: bool,
        message: String,
    },
}

/// One private local-control response. Descriptors are attached out of band.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalControlResponse {
    pub result: Result<LocalControlValue, String>,
}

/// Successful private local-control values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalControlValue {
    Handshake {
        protocol_version: u32,
        server_pid: u32,
    },
    Presentation {
        generation: u64,
        active_session: u64,
        frame_transport: FrameTransportKind,
    },
    DmabufResource(LocalDmabufResource),
    SharedMemoryResource {
        app_id: String,
    },
    Bool(bool),
    Control(String),
    RealmArtifactRequests(Vec<(u64, String)>),
}

/// Metadata accompanying one DMA-BUF plane descriptor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalDmabufResource {
    pub sequence: u64,
    pub app_id: String,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub color_space: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
}
