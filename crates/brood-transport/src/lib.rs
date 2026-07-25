//! Presentation carriers and transport-specific frame payloads.

mod artifact;
mod network;

pub use artifact::{
    ArtifactExchange, ArtifactExchangeEvent, ArtifactExchangeUpdate, ArtifactOffer,
    InstalledArtifact, default_artifact_cache, digest_file, digest_hex,
};
pub use network::{
    ClientHello, NetworkConnection, NetworkFramePacket, NetworkLane, NetworkReceiveLane,
    NetworkSendLane, ServerHello, accept_network_connection, client_endpoint, connect_network,
    server_endpoint,
};
