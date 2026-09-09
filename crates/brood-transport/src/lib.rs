//! Presentation carriers and transport-specific frame payloads.

mod artifact;
mod network;

pub use artifact::{
    ArtifactExchange, ArtifactExchangeEvent, ArtifactExchangeUpdate, ArtifactOffer, ArtifactSender,
    ArtifactTransfer, ArtifactTransferMessages, InstalledArtifact, default_artifact_cache,
    digest_file, digest_hex, package_offers,
};
pub use network::{
    ClientHello, NetworkConnection, NetworkFramePacket, NetworkLane, NetworkReceiveLane,
    NetworkSendLane, ServerHello, accept_network_connection, client_endpoint, connect_network,
    server_endpoint,
};
