//! Transport-neutral presentation protocol.
//!
//! This crate owns the semantic messages exchanged by a session server and a
//! presentation client. Local shared-memory records and remote wire frames are
//! codecs for these types; neither carrier defines presentation behavior.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current semantic presentation protocol version.
pub const PRESENTATION_PROTOCOL_VERSION: u32 = 21;

/// Maximum actor-message body accepted by every transport profile.
pub const MAX_ACTOR_MESSAGE_BYTES: u32 = 64 * 1024;

/// Stable identity of one independently controlled session backend.
///
/// Pane and session numbers are only unique within a backend. Presentation
/// clients must retain this identity whenever they cache or route backend
/// state, even when a connection currently carries only one backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BackendId(pub u64);

/// Version of the content-addressed artifact manifest contract.
pub const ARTIFACT_ABI_VERSION: u32 = 1;

/// Maximum accepted size of one transferred executable artifact.
pub const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum payload carried by one artifact chunk.
pub const MAX_ARTIFACT_CHUNK_BYTES: usize = 16 * 1024;

/// Maximum stable artifact identity length.
pub const MAX_ARTIFACT_ID_BYTES: usize = 128;

/// Maximum package-relative artifact path length.
pub const MAX_ARTIFACT_PACKAGE_PATH_BYTES: usize = 512;

/// Maximum number of requested capability names in one artifact manifest.
pub const MAX_ARTIFACT_CAPABILITIES: usize = 32;

/// Maximum capability-name length.
pub const MAX_ARTIFACT_CAPABILITY_BYTES: usize = 128;

/// Maximum peer-facing artifact rejection detail.
pub const MAX_ARTIFACT_REJECTION_BYTES: usize = 512;

/// One logical traffic class independently mapped by each carrier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lane {
    /// Lossless latency-sensitive state transitions such as key changes.
    Critical,
    /// Lossless ordered session-actor traffic, sharded by session identity.
    InteractiveSession0,
    InteractiveSession1,
    InteractiveSession2,
    InteractiveSession3,
    /// Lossless ordered daemon-actor traffic isolated from session interaction.
    InteractiveDaemon,
    /// Lossless ordered inventory and resource transitions.
    Reliable,
    /// Supersedable state where only the newest generation matters.
    LatestState,
    /// Bounded large payloads that must not block critical traffic.
    Bulk,
    /// Frame resource registration, publication, and release.
    FrameResource,
}

/// Carrier used for ordinary presentation protocol messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CarrierKind {
    /// Size-sealed shared-memory lanes with descriptor-broker bootstrap.
    LocalSharedMemory,
    /// Authenticated encrypted network connection.
    Network,
}

/// Backing used for managed-window frame content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameTransportKind {
    /// Same-device Linux DMA-BUF resources.
    Dmabuf,
    /// Same-machine shared-memory image resources.
    SharedMemory,
    /// Encoded full or damaged image content over the network carrier.
    Network,
}

impl From<FrameTransportKind> for u32 {
    fn from(value: FrameTransportKind) -> Self {
        match value {
            FrameTransportKind::Dmabuf => 1,
            FrameTransportKind::SharedMemory => 2,
            FrameTransportKind::Network => 3,
        }
    }
}

impl TryFrom<u32> for FrameTransportKind {
    type Error = UnsupportedFrameTransport;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Dmabuf),
            2 => Ok(Self::SharedMemory),
            3 => Ok(Self::Network),
            _ => Err(UnsupportedFrameTransport(value)),
        }
    }
}

/// A handshake offered a frame transport unknown to this protocol version.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unsupported frame transport {0}")]
pub struct UnsupportedFrameTransport(pub u32);

/// A selected connection and frame-transport combination.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransportProfile {
    pub carrier: CarrierKind,
    pub frames: FrameTransportKind,
}

impl TransportProfile {
    /// Preferred same-device zero-copy profile.
    pub const LOCAL_DMABUF: Self = Self {
        carrier: CarrierKind::LocalSharedMemory,
        frames: FrameTransportKind::Dmabuf,
    };

    /// Same-machine copied-image correctness profile.
    pub const LOCAL_SHARED_MEMORY: Self = Self {
        carrier: CarrierKind::LocalSharedMemory,
        frames: FrameTransportKind::SharedMemory,
    };

    /// Cross-machine presentation profile.
    pub const REMOTE: Self = Self {
        carrier: CarrierKind::Network,
        frames: FrameTransportKind::Network,
    };
}

/// Immutable capabilities offered during connection establishment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EndpointCapabilities {
    pub protocol_version: u32,
    pub carriers: Vec<CarrierKind>,
    pub frame_transports: Vec<FrameTransportKind>,
    pub max_reliable_message_bytes: u32,
    pub max_frame_bytes: u64,
}

impl EndpointCapabilities {
    /// Capabilities for the Linux-native local session server.
    #[must_use]
    pub fn local_server(dmabuf: bool) -> Self {
        let mut frame_transports = Vec::with_capacity(2);
        if dmabuf {
            frame_transports.push(FrameTransportKind::Dmabuf);
        }
        frame_transports.push(FrameTransportKind::SharedMemory);
        Self {
            protocol_version: PRESENTATION_PROTOCOL_VERSION,
            carriers: vec![CarrierKind::LocalSharedMemory],
            frame_transports,
            max_reliable_message_bytes: MAX_ACTOR_MESSAGE_BYTES,
            max_frame_bytes: u64::from(u32::MAX),
        }
    }

    /// Capabilities for a remote presentation endpoint.
    #[must_use]
    pub fn remote() -> Self {
        Self {
            protocol_version: PRESENTATION_PROTOCOL_VERSION,
            carriers: vec![CarrierKind::Network],
            frame_transports: vec![FrameTransportKind::Network],
            max_reliable_message_bytes: MAX_ACTOR_MESSAGE_BYTES,
            max_frame_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Why two endpoints could not establish a presentation profile.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NegotiationError {
    #[error("presentation protocol versions do not match")]
    ProtocolVersion,
    #[error("endpoints have no common connection carrier")]
    Carrier,
    #[error("endpoints have no common frame transport for the selected carrier")]
    FrameTransport,
}

/// Select an immutable profile from two endpoint offers.
///
/// DMA-BUF is preferred only on the local carrier. Shared-memory images are
/// the local correctness baseline, while network carriers require network
/// frames even if both machines individually support DMA-BUF.
pub fn negotiate_profile(
    client: &EndpointCapabilities,
    server: &EndpointCapabilities,
) -> Result<TransportProfile, NegotiationError> {
    if client.protocol_version != server.protocol_version {
        return Err(NegotiationError::ProtocolVersion);
    }

    let common = |kind| client.carriers.contains(&kind) && server.carriers.contains(&kind);
    if common(CarrierKind::LocalSharedMemory) {
        for frames in [FrameTransportKind::Dmabuf, FrameTransportKind::SharedMemory] {
            if client.frame_transports.contains(&frames)
                && server.frame_transports.contains(&frames)
            {
                return Ok(TransportProfile {
                    carrier: CarrierKind::LocalSharedMemory,
                    frames,
                });
            }
        }
        return Err(NegotiationError::FrameTransport);
    }

    if common(CarrierKind::Network) {
        if client
            .frame_transports
            .contains(&FrameTransportKind::Network)
            && server
                .frame_transports
                .contains(&FrameTransportKind::Network)
        {
            return Ok(TransportProfile::REMOTE);
        }
        return Err(NegotiationError::FrameTransport);
    }

    Err(NegotiationError::Carrier)
}

/// Color encoding carried by one managed-window frame.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u32)]
pub enum FrameColorSpace {
    /// BT.709 primaries, D65 white point, and the sRGB transfer function.
    Srgb = 1,
}

impl From<FrameColorSpace> for u32 {
    fn from(value: FrameColorSpace) -> Self {
        value as Self
    }
}

impl TryFrom<u32> for FrameColorSpace {
    type Error = UnsupportedFrameColorSpace;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Srgb),
            _ => Err(UnsupportedFrameColorSpace(value)),
        }
    }
}

/// A frame resource used a color-space identifier unknown to this endpoint.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unsupported frame color space {0}")]
pub struct UnsupportedFrameColorSpace(pub u32);

/// Packed pixel format used by copied image transports.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u32)]
pub enum FramePixelFormat {
    /// Red, green, blue, alpha byte order.
    Rgba8 = 1,
    /// Blue, green, red, alpha byte order.
    Bgra8 = 2,
}

impl From<FramePixelFormat> for u32 {
    fn from(value: FramePixelFormat) -> Self {
        value as Self
    }
}

impl TryFrom<u32> for FramePixelFormat {
    type Error = UnsupportedFramePixelFormat;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Rgba8),
            2 => Ok(Self::Bgra8),
            _ => Err(UnsupportedFramePixelFormat(value)),
        }
    }
}

/// A copied image used a pixel format unknown to this endpoint.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unsupported frame pixel format {0}")]
pub struct UnsupportedFramePixelFormat(pub u32);

/// One complete revisioned managed-window catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogSnapshot {
    pub backend: BackendId,
    pub revision: u64,
    pub active_session: u64,
    pub sessions: Vec<CatalogSession>,
    pub windows: Vec<CatalogWindow>,
}

/// One session in a catalog snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogSession {
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

/// One managed window in a catalog snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogWindow {
    pub pane: u64,
    pub session: u64,
    pub app_id: String,
}

/// Pointer position in pane-local logical coordinates.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PanePosition {
    pub pane: u64,
    pub x: f64,
    pub y: f64,
}

/// Lossless and supersedable input sent from a presentation client.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputEvent {
    KeyboardKey {
        keycode: u32,
        pressed: bool,
    },
    KeyboardReset,
    PresentationSize {
        width: u32,
        height: u32,
        refresh_millihz: u32,
    },
    PresentationFocus {
        focused: bool,
    },
    WindowFocus {
        pane: u64,
    },
    WindowResize {
        pane: u64,
        width: u32,
        height: u32,
    },
    PointerMotion {
        position: PanePosition,
    },
    PointerButton {
        position: PanePosition,
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        position: PanePosition,
        horizontal: f64,
        vertical: f64,
    },
    PointerWheel {
        position: PanePosition,
        horizontal_v120: i32,
        vertical_v120: i32,
    },
    PointerLeave,
}

impl InputEvent {
    /// Logical carrier lane required for this event.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        match self {
            Self::PointerMotion { .. } => Lane::LatestState,
            Self::KeyboardKey { .. }
            | Self::KeyboardReset
            | Self::PresentationSize { .. }
            | Self::PresentationFocus { .. }
            | Self::WindowFocus { .. }
            | Self::WindowResize { .. }
            | Self::PointerButton { .. }
            | Self::PointerAxis { .. }
            | Self::PointerWheel { .. }
            | Self::PointerLeave => Lane::Critical,
        }
    }
}

/// Explicit client-side lifecycle operations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientLifecycle {
    FramePresented { sequence: u64 },
    Disconnect,
}

/// Content digest used as an artifact's immutable network identity.
pub type ArtifactDigest = [u8; 32];

/// Runtime realm in which an artifact requests installation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactRealm {
    Client,
    Session,
}

/// Executable artifact shape supported by the baseline exchange.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    WasmActor,
}

/// Placement of one artifact inside a lazily hydrated package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactPackageMember {
    pub package: String,
    pub path: String,
    pub root: bool,
}

impl ArtifactPackageMember {
    fn validate(&self) -> Result<(), ArtifactValidationError> {
        if !valid_artifact_id(&self.package) {
            return Err(ArtifactValidationError::Id);
        }
        let path = std::path::Path::new(&self.path);
        if self.path.is_empty()
            || self.path.len() > MAX_ARTIFACT_PACKAGE_PATH_BYTES
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(ArtifactValidationError::PackagePath);
        }
        Ok(())
    }
}

/// Immutable content-addressed artifact declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactManifest {
    pub id: String,
    pub realm: ArtifactRealm,
    pub kind: ArtifactKind,
    pub digest: ArtifactDigest,
    pub bytes: u64,
    pub abi_version: u32,
    pub capabilities: Vec<String>,
    pub package: Option<ArtifactPackageMember>,
}

impl ArtifactManifest {
    /// Validate all peer-controlled manifest bounds and identifiers.
    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        if !valid_artifact_id(&self.id) {
            return Err(ArtifactValidationError::Id);
        }
        if self.bytes == 0 || self.bytes > MAX_ARTIFACT_BYTES {
            return Err(ArtifactValidationError::Size(self.bytes));
        }
        if self.abi_version != ARTIFACT_ABI_VERSION {
            return Err(ArtifactValidationError::Abi(self.abi_version));
        }
        if self.capabilities.len() > MAX_ARTIFACT_CAPABILITIES
            || self.capabilities.iter().any(|capability| {
                capability.is_empty() || capability.len() > MAX_ARTIFACT_CAPABILITY_BYTES
            })
        {
            return Err(ArtifactValidationError::Capabilities);
        }
        if let Some(package) = &self.package {
            package.validate()?;
        }
        Ok(())
    }
}

fn valid_artifact_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ARTIFACT_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// One symmetric artifact-plane message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactMessage {
    Offer(ArtifactManifest),
    Request {
        digest: ArtifactDigest,
    },
    Chunk {
        digest: ArtifactDigest,
        offset: u64,
        bytes: Vec<u8>,
    },
    Complete {
        digest: ArtifactDigest,
    },
    Accepted {
        digest: ArtifactDigest,
    },
    Rejected {
        digest: ArtifactDigest,
        reason: String,
    },
}

impl ArtifactMessage {
    /// Validate bounded peer-controlled artifact message fields.
    pub fn validate(&self) -> Result<(), ArtifactValidationError> {
        match self {
            Self::Offer(manifest) => manifest.validate(),
            Self::Chunk { bytes, .. } if bytes.len() > MAX_ARTIFACT_CHUNK_BYTES => {
                Err(ArtifactValidationError::Chunk(bytes.len()))
            }
            Self::Rejected { reason, .. } if reason.len() > MAX_ARTIFACT_REJECTION_BYTES => {
                Err(ArtifactValidationError::Rejection(reason.len()))
            }
            Self::Request { .. }
            | Self::Chunk { .. }
            | Self::Complete { .. }
            | Self::Accepted { .. }
            | Self::Rejected { .. } => Ok(()),
        }
    }
}

/// Invalid peer-controlled artifact metadata.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ArtifactValidationError {
    #[error("artifact id is empty, oversized, or contains unsupported characters")]
    Id,
    #[error("artifact size {0} is outside the supported bounds")]
    Size(u64),
    #[error("artifact ABI version {0} is unsupported")]
    Abi(u32),
    #[error("artifact capability declarations exceed the supported bounds")]
    Capabilities,
    #[error("artifact package path is empty, oversized, absolute, or escapes its package")]
    PackagePath,
    #[error("artifact chunk size {0} exceeds the supported bound")]
    Chunk(usize),
    #[error("artifact rejection detail size {0} exceeds the supported bound")]
    Rejection(usize),
}

/// Message sent from a presentation client to its session server.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientMessage {
    Input(InputEvent),
    Lifecycle(ClientLifecycle),
    Actor(ActorMessage),
    BackendRequest(BackendRequest),
    Artifact(ArtifactMessage),
    ReleaseResource { resource: u64 },
}

/// Backend actor realm addressed by a presentation connection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendActorRealm {
    Daemon,
    Session(u64),
}

/// Addressed message crossing between presentation and backend actor realms.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActorMessage {
    pub realm: BackendActorRealm,
    pub sender: String,
    pub recipient: String,
    pub body: Vec<u8>,
}

/// One asynchronous general-backend request from a presentation client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendRequest {
    pub id: u64,
    pub operation: BackendOperation,
}

/// Transport-neutral backend operations needed by presentation actors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendOperation {
    /// Execute one ordinary Brood control command.
    Control { argv: Vec<String> },
    /// Change or refresh the surface-session catalog.
    Session {
        operation: SessionOperation,
        session: Option<u64>,
        name: Option<String>,
        directory: Option<String>,
    },
}

/// Surface-session operation carried independently from the WASM ABI.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionOperation {
    Refresh,
    Activate,
    Create,
    Rename,
    Close,
}

/// Result of one asynchronous general-backend request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendResponse {
    pub id: u64,
    pub result: Result<BackendResponseValue, String>,
}

/// Successful value returned by a general-backend request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendResponseValue {
    Control(String),
    Session(Option<u64>),
}

/// Transport-independent frame-resource reference.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameResourceRef {
    Dmabuf { resource: u64 },
    SharedMemory { resource: u64 },
    Network { resource: u64 },
}

impl FrameResourceRef {
    /// Generation-local resource identifier.
    #[must_use]
    pub const fn id(self) -> u64 {
        match self {
            Self::Dmabuf { resource }
            | Self::SharedMemory { resource }
            | Self::Network { resource } => resource,
        }
    }
}

/// One managed-window frame in an atomic scene update.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WindowFrame {
    pub pane: u64,
    pub resource: FrameResourceRef,
    pub source: [i32; 4],
    pub active: bool,
}

/// One atomic collection of independently transported managed-window frames.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrameScene {
    pub backend: BackendId,
    pub sequence: u64,
    pub extent: [u32; 2],
    pub windows: Vec<WindowFrame>,
}

/// Message sent from a session server to a presentation client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerMessage {
    Catalog(CatalogSnapshot),
    Scene(FrameScene),
    Actor(ActorMessage),
    BackendResponse(BackendResponse),
    Artifact(ArtifactMessage),
    FocusPresentation,
    ClosePresentation,
    DisconnectAcknowledged,
}

#[cfg(test)]
mod tests {
    use super::{
        ARTIFACT_ABI_VERSION, ArtifactKind, ArtifactManifest, ArtifactMessage, ArtifactRealm,
        ArtifactValidationError, CarrierKind, EndpointCapabilities, FrameTransportKind,
        MAX_ARTIFACT_CHUNK_BYTES, NegotiationError, PRESENTATION_PROTOCOL_VERSION,
        TransportProfile, negotiate_profile,
    };

    #[test]
    fn local_negotiation_prefers_dmabuf() {
        let client = EndpointCapabilities::local_server(true);
        let server = EndpointCapabilities::local_server(true);
        assert_eq!(
            negotiate_profile(&client, &server),
            Ok(TransportProfile::LOCAL_DMABUF)
        );
    }

    #[test]
    fn local_negotiation_falls_back_to_shared_memory() {
        let mut client = EndpointCapabilities::local_server(false);
        client.carriers = vec![CarrierKind::LocalSharedMemory];
        let server = EndpointCapabilities::local_server(true);
        assert_eq!(
            negotiate_profile(&client, &server),
            Ok(TransportProfile::LOCAL_SHARED_MEMORY)
        );
    }

    #[test]
    fn remote_never_selects_a_local_frame_transport() {
        let client = EndpointCapabilities {
            protocol_version: PRESENTATION_PROTOCOL_VERSION,
            carriers: vec![CarrierKind::Network],
            frame_transports: vec![FrameTransportKind::Dmabuf, FrameTransportKind::Network],
            max_reliable_message_bytes: 1,
            max_frame_bytes: 1,
        };
        let server = client.clone();
        assert_eq!(
            negotiate_profile(&client, &server),
            Ok(TransportProfile::REMOTE)
        );
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let client = EndpointCapabilities::local_server(true);
        let mut server = client.clone();
        server.protocol_version = server.protocol_version.saturating_add(1);
        assert_eq!(
            negotiate_profile(&client, &server),
            Err(NegotiationError::ProtocolVersion)
        );
    }

    #[test]
    fn artifact_metadata_and_chunks_are_bounded_before_exchange() {
        let manifest = ArtifactManifest {
            id: "workspace.ui".to_string(),
            realm: ArtifactRealm::Client,
            kind: ArtifactKind::WasmActor,
            digest: [7; 32],
            bytes: 42,
            abi_version: ARTIFACT_ABI_VERSION,
            capabilities: Vec::new(),
            package: None,
        };
        assert_eq!(manifest.validate(), Ok(()));

        let mut invalid = manifest;
        invalid.id = "../escape".to_string();
        assert_eq!(invalid.validate(), Err(ArtifactValidationError::Id));
        assert_eq!(
            ArtifactMessage::Chunk {
                digest: [7; 32],
                offset: 0,
                bytes: vec![0; MAX_ARTIFACT_CHUNK_BYTES + 1],
            }
            .validate(),
            Err(ArtifactValidationError::Chunk(MAX_ARTIFACT_CHUNK_BYTES + 1))
        );
    }
}
