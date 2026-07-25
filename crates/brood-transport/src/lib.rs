//! Presentation carriers and transport-specific frame payloads.

mod network;

pub use network::{
    ClientHello, NetworkConnection, NetworkFramePacket, NetworkLane, NetworkReceiveLane,
    NetworkSendLane, ServerHello, accept_network_connection, client_endpoint, connect_network,
    server_endpoint,
};
