use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use brood_protocol::{
    BackendActorRealm, CarrierKind, ClientMessage, EndpointCapabilities, FrameColorSpace,
    FramePixelFormat, Lane, MAX_ACTOR_ID_BYTES, MAX_ACTOR_MESSAGE_BYTES, MAX_NETWORK_FRAME_BYTES,
    ServerMessage, TransportProfile, managed_resize_epoch_is_valid, negotiate_profile,
};
use quinn::{
    AckFrequencyConfig, Connection, Endpoint, Incoming, RecvStream, SendStream, VarInt,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Create a mutually authenticated QUIC listener.
pub fn server_endpoint(
    bind_address: SocketAddr,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    trusted_client_certificate_der: Vec<u8>,
) -> Result<Endpoint> {
    let certificate = rustls::pki_types::CertificateDer::from(certificate_der);
    let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(private_key_der).into();
    let mut client_roots = rustls::RootCertStore::empty();
    client_roots
        .add(rustls::pki_types::CertificateDer::from(
            trusted_client_certificate_der,
        ))
        .context("add trusted presentation client certificate")?;
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .context("configure presentation client certificate verifier")?;
    let mut tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![certificate], private_key)
        .context("configure QUIC server identity")?;
    tls.alpn_protocols = vec![presentation_alpn()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(tls).context("configure QUIC server TLS")?,
    ));
    config.transport_config(Arc::new(network_transport_config()));
    Endpoint::server(config, bind_address).context("bind QUIC presentation listener")
}

/// Create a QUIC client pinned to one server and presenting its own identity.
pub fn client_endpoint(
    bind_address: SocketAddr,
    trusted_certificate_der: Vec<u8>,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
) -> Result<Endpoint> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            trusted_certificate_der,
        ))
        .context("add trusted presentation server certificate")?;
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![rustls::pki_types::CertificateDer::from(certificate_der)],
            rustls::pki_types::PrivatePkcs8KeyDer::from(private_key_der).into(),
        )
        .context("configure QUIC presentation client identity")?;
    tls.alpn_protocols = vec![presentation_alpn()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).context("configure QUIC client TLS")?,
    ));
    config.transport_config(Arc::new(network_transport_config()));
    let mut endpoint =
        Endpoint::client(bind_address).context("bind QUIC presentation client socket")?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

fn network_transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams(VarInt::from_u32(MAX_PEER_BIDIRECTIONAL_STREAMS));
    config.max_concurrent_uni_streams(VarInt::from_u32(0));
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    let mut acknowledgements = AckFrequencyConfig::default();
    acknowledgements
        .ack_eliciting_threshold(VarInt::from_u32(0))
        .max_ack_delay(Some(Duration::from_millis(2)));
    config.ack_frequency_config(Some(acknowledgements));
    config
}

fn presentation_alpn() -> Vec<u8> {
    format!(
        "brood-presentation/{}",
        brood_protocol::PRESENTATION_PROTOCOL_VERSION
    )
    .into_bytes()
}

const LANES: [Lane; 10] = [
    Lane::Critical,
    Lane::InteractiveSession0,
    Lane::InteractiveSession1,
    Lane::InteractiveSession2,
    Lane::InteractiveSession3,
    Lane::InteractiveDaemon,
    Lane::Reliable,
    Lane::LatestState,
    Lane::Bulk,
    Lane::FrameResource,
];
// Artifact transfer is chunked at the protocol layer, but a remote backend
// may still emit a large compressed frame resource. Keep the carrier ceiling
// above the largest valid artifact/package exchange while retaining a hard
// bound against untrusted length prefixes.
const FRAME_ENVELOPE_BYTES: usize = 64 * 1024;
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
const MAX_WIRE_MESSAGE_BYTES: usize = 64 * 1024 * 1024 + FRAME_ENVELOPE_BYTES;
const MAX_FRAME_DAMAGE_RECTS: usize = 1_024;
const LANE_FINISH_TIMEOUT: Duration = Duration::from_secs(5);
// One handshake stream plus the ten fixed semantic lanes.
const MAX_PEER_BIDIRECTIONAL_STREAMS: u32 = 11;
const _: () = assert!(LANES.len() == 10);

/// First authenticated application message sent by the remote client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    pub backend: brood_protocol::BackendId,
    pub capabilities: EndpointCapabilities,
}

/// Server-selected immutable profile and connection generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerHello {
    pub backend: brood_protocol::BackendId,
    pub profile: TransportProfile,
    pub connection_generation: u64,
    pub capabilities: EndpointCapabilities,
}

/// One independently ordered semantic QUIC stream pair.
pub struct NetworkLane {
    connection: Connection,
    lane: Lane,
    send: SendStream,
    receive: RecvStream,
    send_buffer: Vec<u8>,
    receive_buffer: Vec<u8>,
    send_limit: usize,
    receive_limit: usize,
    send_frame_limit: u64,
    receive_frame_limit: u64,
}

/// Sending half of one semantic QUIC lane.
pub struct NetworkSendLane {
    _connection: Connection,
    lane: Lane,
    send: SendStream,
    buffer: Vec<u8>,
    limit: usize,
    frame_limit: u64,
}

/// Receiving half of one semantic QUIC lane.
pub struct NetworkReceiveLane {
    _connection: Connection,
    lane: Lane,
    receive: RecvStream,
    buffer: Vec<u8>,
    limit: usize,
    frame_limit: u64,
}

impl NetworkLane {
    /// Separate send and receive ownership for independent I/O tasks.
    #[must_use]
    pub fn split(self) -> (NetworkSendLane, NetworkReceiveLane) {
        (
            NetworkSendLane {
                _connection: self.connection.clone(),
                lane: self.lane,
                send: self.send,
                buffer: self.send_buffer,
                limit: self.send_limit,
                frame_limit: self.send_frame_limit,
            },
            NetworkReceiveLane {
                _connection: self.connection,
                lane: self.lane,
                receive: self.receive,
                buffer: self.receive_buffer,
                limit: self.receive_limit,
                frame_limit: self.receive_frame_limit,
            },
        )
    }

    /// Logical traffic class carried by this stream pair.
    #[must_use]
    pub const fn kind(&self) -> Lane {
        self.lane
    }

    /// Send a client message after validating its semantic lane.
    pub async fn send_client_message(&mut self, message: &ClientMessage) -> Result<()> {
        ensure!(
            client_message_lane(message) == self.lane,
            "client message belongs on a different semantic lane"
        );
        validate_client_message(message)?;
        self.send_value(message).await
    }

    /// Receive and validate a client message.
    pub async fn receive_client_message(&mut self) -> Result<ClientMessage> {
        let message = self.receive_value().await?;
        ensure!(
            client_message_lane(&message) == self.lane,
            "client message arrived on the wrong semantic lane"
        );
        validate_client_message(&message)?;
        Ok(message)
    }

    /// Send a server message after validating its semantic lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        ensure!(
            server_message_lane(message) == self.lane,
            "server message belongs on a different semantic lane"
        );
        validate_server_message(message)?;
        self.send_value(message).await
    }

    /// Receive and validate a server message.
    pub async fn receive_server_message(&mut self) -> Result<ServerMessage> {
        let message = self.receive_value().await?;
        ensure!(
            server_message_lane(&message) == self.lane,
            "server message arrived on the wrong semantic lane"
        );
        validate_server_message(&message)?;
        Ok(message)
    }

    /// Send a compressed frame on the frame-resource lane.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        frame.validate(self.send_frame_limit)?;
        self.send_value(frame).await
    }

    /// Receive a compressed frame from the frame-resource lane.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        let frame: NetworkFramePacket = self.receive_value().await?;
        frame.validate(self.receive_frame_limit)?;
        Ok(frame)
    }

    async fn send_value<T: Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        send_value(
            &mut self.send,
            &mut self.send_buffer,
            self.send_limit,
            value,
        )
        .await
    }

    async fn receive_value<T: DeserializeOwned>(&mut self) -> Result<T> {
        receive_value(
            &mut self.receive,
            &mut self.receive_buffer,
            self.receive_limit,
        )
        .await
    }
}

impl NetworkSendLane {
    /// Send a client message after validating its semantic lane.
    pub async fn send_client_message(&mut self, message: &ClientMessage) -> Result<()> {
        ensure!(
            client_message_lane(message) == self.lane,
            "client message belongs on a different semantic lane"
        );
        validate_client_message(message)?;
        send_value(&mut self.send, &mut self.buffer, self.limit, message).await
    }

    /// Send a server message after validating its semantic lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        ensure!(
            server_message_lane(message) == self.lane,
            "server message belongs on a different semantic lane"
        );
        validate_server_message(message)?;
        send_value(&mut self.send, &mut self.buffer, self.limit, message).await
    }

    /// Send a compressed frame on the frame-resource lane.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        frame.validate(self.frame_limit)?;
        send_value(&mut self.send, &mut self.buffer, self.limit, frame).await
    }

    /// Finish this lane and wait until the peer acknowledges every buffered byte.
    pub async fn finish(&mut self) -> Result<()> {
        self.send
            .finish()
            .context("finish presentation network lane")?;
        let _peer_stop = tokio::time::timeout(LANE_FINISH_TIMEOUT, self.send.stopped())
            .await
            .context("presentation network lane acknowledgement timed out")?
            .context("wait for presentation network lane acknowledgement")?;
        Ok(())
    }
}

impl NetworkReceiveLane {
    /// Receive and validate a client message.
    pub async fn receive_client_message(&mut self) -> Result<ClientMessage> {
        let message = receive_value(&mut self.receive, &mut self.buffer, self.limit).await?;
        ensure!(
            client_message_lane(&message) == self.lane,
            "client message arrived on the wrong semantic lane"
        );
        validate_client_message(&message)?;
        Ok(message)
    }

    /// Receive and validate a server message.
    pub async fn receive_server_message(&mut self) -> Result<ServerMessage> {
        let message = receive_value(&mut self.receive, &mut self.buffer, self.limit).await?;
        ensure!(
            server_message_lane(&message) == self.lane,
            "server message arrived on the wrong semantic lane"
        );
        validate_server_message(&message)?;
        Ok(message)
    }

    /// Receive a compressed frame from the frame-resource lane.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        let frame: NetworkFramePacket =
            receive_value(&mut self.receive, &mut self.buffer, self.limit).await?;
        frame.validate(self.frame_limit)?;
        Ok(frame)
    }

    /// Read the peer's lane finish marker after its final framed message.
    pub async fn wait_for_finish(&mut self) -> Result<()> {
        let trailing = tokio::time::timeout(LANE_FINISH_TIMEOUT, self.receive.read_to_end(0))
            .await
            .context("presentation network lane finish timed out")?
            .context("wait for presentation network lane finish")?;
        ensure!(
            trailing.is_empty(),
            "peer sent trailing bytes after the final presentation message"
        );
        Ok(())
    }
}

async fn send_value<T: Serialize + Sync>(
    send: &mut SendStream,
    buffer: &mut Vec<u8>,
    limit: usize,
    value: &T,
) -> Result<()> {
    buffer.clear();
    let reusable = std::mem::take(buffer);
    *buffer =
        postcard::to_extend(value, reusable).context("encode presentation network message")?;
    ensure!(
        buffer.len() <= limit,
        "presentation network message exceeds its negotiated lane limit"
    );
    write_frame(send, buffer).await
}

async fn receive_value<T: DeserializeOwned>(
    receive: &mut RecvStream,
    buffer: &mut Vec<u8>,
    limit: usize,
) -> Result<T> {
    read_frame(receive, buffer, limit).await?;
    postcard::from_bytes(buffer).context("decode presentation network message")
}

/// One mutually authenticated QUIC presentation carrier.
///
/// Each semantic traffic class owns a distinct ordered QUIC stream, preventing
/// frame congestion from causing head-of-line blocking for critical input.
pub struct NetworkConnection {
    connection: Connection,
    lanes: HashMap<Lane, NetworkLane>,
    pub backend: brood_protocol::BackendId,
    pub profile: TransportProfile,
    pub connection_generation: u64,
    pub peer_capabilities: EndpointCapabilities,
    local_capabilities: EndpointCapabilities,
}

#[derive(Clone, Copy, Debug)]
pub struct NetworkDiagnostics {
    pub rtt_us: u128,
    pub congestion_window: u64,
    pub congestion_events: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub sent_packets: u64,
    pub mtu: u16,
}

impl NetworkConnection {
    #[must_use]
    pub fn diagnostics(&self) -> NetworkDiagnostics {
        let stats = self.connection.stats();
        NetworkDiagnostics {
            rtt_us: stats.path.rtt.as_micros(),
            congestion_window: stats.path.cwnd,
            congestion_events: stats.path.congestion_events,
            lost_packets: stats.path.lost_packets,
            lost_bytes: stats.path.lost_bytes,
            sent_packets: stats.path.sent_packets,
            mtu: stats.path.current_mtu,
        }
    }

    /// Detach one semantic lane for an independent I/O task.
    pub fn take_lane(&mut self, lane: Lane) -> Result<NetworkLane> {
        self.lanes
            .remove(&lane)
            .context("presentation network lane is unavailable")
    }

    /// Send one client message on its required logical lane.
    pub async fn send_client_message(&mut self, message: &ClientMessage) -> Result<()> {
        validate_client_message(message)?;
        self.send(client_message_lane(message), message).await
    }

    /// Receive one client message from a specified logical lane.
    pub async fn receive_client_message(&mut self, lane: Lane) -> Result<ClientMessage> {
        let message = self.receive(lane).await?;
        ensure!(
            client_message_lane(&message) == lane,
            "client message arrived on the wrong semantic lane"
        );
        validate_client_message(&message)?;
        Ok(message)
    }

    /// Send one server message on its required logical lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        validate_server_message(message)?;
        self.send(server_message_lane(message), message).await
    }

    /// Receive one server message from a specified logical lane.
    pub async fn receive_server_message(&mut self, lane: Lane) -> Result<ServerMessage> {
        let message = self.receive(lane).await?;
        ensure!(
            server_message_lane(&message) == lane,
            "server message arrived on the wrong semantic lane"
        );
        validate_server_message(&message)?;
        Ok(message)
    }

    /// Send one compressed managed-window frame packet.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        frame.validate(self.peer_capabilities.max_frame_bytes)?;
        self.send(Lane::FrameResource, frame).await
    }

    /// Receive one compressed managed-window frame packet.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        let frame: NetworkFramePacket = self.receive(Lane::FrameResource).await?;
        frame.validate(self.local_capabilities.max_frame_bytes)?;
        Ok(frame)
    }

    async fn send<T: Serialize + Sync>(&mut self, lane: Lane, value: &T) -> Result<()> {
        self.lanes
            .get_mut(&lane)
            .context("presentation network lane is unavailable")?
            .send_value(value)
            .await
    }

    async fn receive<T: DeserializeOwned>(&mut self, lane: Lane) -> Result<T> {
        self.lanes
            .get_mut(&lane)
            .context("presentation network lane is unavailable")?
            .receive_value()
            .await
    }
}

/// Connect to a mutually authenticated remote session server.
pub async fn connect_network(
    endpoint: &Endpoint,
    server_address: SocketAddr,
    server_name: &str,
    backend: brood_protocol::BackendId,
    capabilities: EndpointCapabilities,
) -> Result<NetworkConnection> {
    validate_capabilities(&capabilities)?;
    ensure!(
        capabilities.carriers.contains(&CarrierKind::Network),
        "client capabilities do not offer the network carrier"
    );
    let connection = endpoint
        .connect(server_address, server_name)
        .context("start authenticated QUIC connection")?
        .await
        .context("authenticate remote session server")?;
    let (mut handshake_send, mut handshake_receive) = connection
        .open_bi()
        .await
        .context("open presentation handshake stream")?;
    write_serialized(
        &mut handshake_send,
        &ClientHello {
            backend,
            capabilities: capabilities.clone(),
        },
    )
    .await?;
    let hello: ServerHello = read_serialized(&mut handshake_receive).await?;
    ensure!(
        hello.backend != brood_protocol::BackendId(0) && hello.connection_generation != 0,
        "server returned an invalid connection identity"
    );
    validate_capabilities(&hello.capabilities)?;
    let expected_profile = negotiate_profile(&capabilities, &hello.capabilities)
        .context("validate server-selected remote presentation profile")?;
    ensure!(
        hello.profile == expected_profile && hello.profile == TransportProfile::REMOTE,
        "server selected an invalid remote presentation profile"
    );
    ensure!(
        hello.backend == backend,
        "server did not retain the client-assigned backend identity"
    );
    let lanes = open_lanes(&connection, &hello.capabilities, &capabilities).await?;
    Ok(NetworkConnection {
        connection,
        lanes,
        backend: hello.backend,
        profile: hello.profile,
        connection_generation: hello.connection_generation,
        peer_capabilities: hello.capabilities,
        local_capabilities: capabilities,
    })
}

/// Accept one mutually authenticated remote presentation client.
pub async fn accept_network_connection(
    incoming: Incoming,
    capabilities: EndpointCapabilities,
    connection_generation: u64,
) -> Result<NetworkConnection> {
    validate_capabilities(&capabilities)?;
    ensure!(
        capabilities.carriers.contains(&CarrierKind::Network),
        "server capabilities do not offer the network carrier"
    );
    let connection = incoming
        .await
        .context("authenticate remote presentation client")?;
    let (mut handshake_send, mut handshake_receive) = connection
        .accept_bi()
        .await
        .context("accept presentation handshake stream")?;
    let hello: ClientHello = read_serialized(&mut handshake_receive).await?;
    ensure!(
        hello.backend != brood_protocol::BackendId(0),
        "client supplied an invalid backend identity"
    );
    ensure!(
        connection_generation != 0,
        "server supplied an invalid connection generation"
    );
    validate_capabilities(&hello.capabilities)?;
    let profile = negotiate_profile(&hello.capabilities, &capabilities)
        .context("negotiate remote presentation profile")?;
    write_serialized(
        &mut handshake_send,
        &ServerHello {
            backend: hello.backend,
            profile,
            connection_generation,
            capabilities: capabilities.clone(),
        },
    )
    .await?;
    let lanes = accept_lanes(&connection, &hello.capabilities, &capabilities).await?;
    Ok(NetworkConnection {
        connection,
        lanes,
        backend: hello.backend,
        profile,
        connection_generation,
        peer_capabilities: hello.capabilities,
        local_capabilities: capabilities,
    })
}

async fn open_lanes(
    connection: &Connection,
    send_capabilities: &EndpointCapabilities,
    receive_capabilities: &EndpointCapabilities,
) -> Result<HashMap<Lane, NetworkLane>> {
    let mut lanes = HashMap::with_capacity(LANES.len());
    for lane in LANES {
        let (mut send, receive) = connection
            .open_bi()
            .await
            .with_context(|| format!("open {lane:?} presentation stream"))?;
        send.set_priority(lane_priority(lane))
            .with_context(|| format!("prioritize {lane:?} presentation stream"))?;
        send.write_all(&[lane_code(lane)])
            .await
            .with_context(|| format!("identify {lane:?} presentation stream"))?;
        lanes.insert(
            lane,
            NetworkLane {
                connection: connection.clone(),
                lane,
                send,
                receive,
                send_buffer: Vec::new(),
                receive_buffer: Vec::new(),
                send_limit: lane_message_limit(send_capabilities, lane)?,
                receive_limit: lane_message_limit(receive_capabilities, lane)?,
                send_frame_limit: send_capabilities.max_frame_bytes,
                receive_frame_limit: receive_capabilities.max_frame_bytes,
            },
        );
    }
    Ok(lanes)
}

async fn accept_lanes(
    connection: &Connection,
    send_capabilities: &EndpointCapabilities,
    receive_capabilities: &EndpointCapabilities,
) -> Result<HashMap<Lane, NetworkLane>> {
    let mut lanes = HashMap::with_capacity(LANES.len());
    while lanes.len() < LANES.len() {
        let (send, mut receive) = connection
            .accept_bi()
            .await
            .context("accept presentation lane stream")?;
        let mut code = [0_u8; 1];
        receive
            .read_exact(&mut code)
            .await
            .context("read presentation lane identity")?;
        let lane = lane_from_code(code[0])?;
        send.set_priority(lane_priority(lane))
            .with_context(|| format!("prioritize {lane:?} presentation stream"))?;
        ensure!(
            lanes
                .insert(
                    lane,
                    NetworkLane {
                        connection: connection.clone(),
                        lane,
                        send,
                        receive,
                        send_buffer: Vec::new(),
                        receive_buffer: Vec::new(),
                        send_limit: lane_message_limit(send_capabilities, lane)?,
                        receive_limit: lane_message_limit(receive_capabilities, lane)?,
                        send_frame_limit: send_capabilities.max_frame_bytes,
                        receive_frame_limit: receive_capabilities.max_frame_bytes,
                    },
                )
                .is_none(),
            "peer opened the same presentation lane more than once"
        );
    }
    Ok(lanes)
}

async fn write_serialized<T: Serialize + Sync>(stream: &mut SendStream, value: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(value).context("encode presentation handshake")?;
    ensure!(
        bytes.len() <= MAX_HANDSHAKE_BYTES,
        "presentation handshake exceeds its size limit"
    );
    write_frame(stream, &bytes).await
}

async fn read_serialized<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T> {
    let mut bytes = Vec::new();
    read_frame(stream, &mut bytes, MAX_HANDSHAKE_BYTES).await?;
    postcard::from_bytes(&bytes).context("decode presentation handshake")
}

async fn write_frame(stream: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let length = u32::try_from(bytes.len()).context("network frame length exceeds u32")?;
    stream
        .write_all(&length.to_be_bytes())
        .await
        .context("write presentation message length")?;
    stream
        .write_all(bytes)
        .await
        .context("write presentation message body")
}

async fn read_frame(stream: &mut RecvStream, buffer: &mut Vec<u8>, limit: usize) -> Result<()> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .await
        .context("read presentation message length")?;
    let length = checked_frame_length(u32::from_be_bytes(length), limit)?;
    buffer.clear();
    buffer.resize(length, 0);
    stream
        .read_exact(buffer)
        .await
        .context("read presentation message body")?;
    Ok(())
}

fn checked_frame_length(encoded: u32, limit: usize) -> Result<usize> {
    let length = usize::try_from(encoded).context("presentation message length is invalid")?;
    ensure!(
        length <= limit && length <= MAX_WIRE_MESSAGE_BYTES,
        "presentation message length {length} exceeds the negotiated lane limit of {limit} bytes"
    );
    Ok(length)
}

fn validate_capabilities(capabilities: &EndpointCapabilities) -> Result<()> {
    ensure!(
        !capabilities.carriers.is_empty() && capabilities.carriers.len() <= 4,
        "endpoint carrier capability list is invalid"
    );
    ensure!(
        !capabilities.frame_transports.is_empty() && capabilities.frame_transports.len() <= 4,
        "endpoint frame capability list is invalid"
    );
    ensure!(
        capabilities.max_reliable_message_bytes != 0
            && capabilities.max_reliable_message_bytes <= MAX_ACTOR_MESSAGE_BYTES,
        "endpoint reliable-message limit is unsupported"
    );
    ensure!(
        capabilities.max_frame_bytes != 0
            && capabilities.max_frame_bytes <= MAX_NETWORK_FRAME_BYTES,
        "endpoint frame limit is unsupported"
    );
    Ok(())
}

fn lane_message_limit(capabilities: &EndpointCapabilities, lane: Lane) -> Result<usize> {
    if lane == Lane::FrameResource {
        return usize::try_from(capabilities.max_frame_bytes)
            .context("endpoint frame limit exceeds address space")?
            .checked_add(FRAME_ENVELOPE_BYTES)
            .context("endpoint frame envelope limit overflow");
    }
    usize::try_from(capabilities.max_reliable_message_bytes)
        .context("endpoint message limit exceeds address space")
}

fn validate_actor_message(message: &brood_protocol::ActorMessage) -> Result<()> {
    ensure!(
        !matches!(message.realm, BackendActorRealm::Session(0)),
        "network actor session identity is invalid"
    );
    ensure!(
        !message.sender.is_empty()
            && message.sender.len() <= MAX_ACTOR_ID_BYTES
            && !message.recipient.is_empty()
            && message.recipient.len() <= MAX_ACTOR_ID_BYTES,
        "network actor identity is invalid"
    );
    ensure!(
        message.body.len() <= MAX_ACTOR_MESSAGE_BYTES as usize,
        "network actor body exceeds the negotiated limit"
    );
    Ok(())
}

fn validate_client_message(message: &ClientMessage) -> Result<()> {
    match message {
        ClientMessage::Input(input) => validate_input(input),
        ClientMessage::Actor(message) => validate_actor_message(message),
        ClientMessage::BackendRequest(request) => validate_backend_request(request),
        ClientMessage::Artifact(message) => message
            .validate()
            .context("validate network artifact message"),
        ClientMessage::Lifecycle(brood_protocol::ClientLifecycle::FramePresented { sequence }) => {
            ensure!(*sequence != 0, "presented frame sequence is invalid");
            Ok(())
        }
        ClientMessage::ReleaseResource { resource } => {
            ensure!(*resource != 0, "released frame resource is invalid");
            Ok(())
        }
        ClientMessage::Lifecycle(brood_protocol::ClientLifecycle::Disconnect) => Ok(()),
    }
}

fn validate_server_message(message: &ServerMessage) -> Result<()> {
    match message {
        ServerMessage::Catalog(catalog) => validate_catalog(catalog),
        ServerMessage::Scene(scene) => validate_scene(scene),
        ServerMessage::Actor(message) => validate_actor_message(message),
        ServerMessage::Artifact(message) => message
            .validate()
            .context("validate network artifact message"),
        ServerMessage::BackendResponse(response) => {
            ensure!(response.id != 0, "backend response identity is invalid");
            if let Err(reason) = &response.result {
                ensure!(reason.len() <= MAX_ACTOR_MESSAGE_BYTES as usize);
            }
            Ok(())
        }
        ServerMessage::ManagedResizeStatus(status) => {
            ensure!(status.pane != 0 && managed_resize_epoch_is_valid(status.resize_epoch));
            if let brood_protocol::ManagedResizeStatusResult::Configured {
                logical_size,
                pixel_size,
                scale_120,
            } = status.result
            {
                ensure!(
                    !logical_size.contains(&0) && !pixel_size.contains(&0) && scale_120 != 0,
                    "managed resize status has invalid units"
                );
            }
            Ok(())
        }
        ServerMessage::FocusPresentation
        | ServerMessage::ClosePresentation
        | ServerMessage::DisconnectAcknowledged => Ok(()),
    }
}

fn validate_input(input: &brood_protocol::InputEvent) -> Result<()> {
    let finite_position = |position: &brood_protocol::PanePosition| {
        position.pane != 0 && position.x.is_finite() && position.y.is_finite()
    };
    match input {
        brood_protocol::InputEvent::PointerMotion { position }
        | brood_protocol::InputEvent::PointerButton { position, .. }
        | brood_protocol::InputEvent::PointerWheel { position, .. } => {
            ensure!(
                finite_position(position),
                "network pointer position is not finite"
            );
        }
        brood_protocol::InputEvent::PointerAxis {
            position,
            horizontal,
            vertical,
        } => {
            ensure!(
                finite_position(position) && horizontal.is_finite() && vertical.is_finite(),
                "network pointer axis is not finite"
            );
        }
        brood_protocol::InputEvent::PresentationSize {
            width,
            height,
            scale_120,
            ..
        } => {
            ensure!(*width != 0 && *height != 0 && *scale_120 != 0);
        }
        brood_protocol::InputEvent::WindowResize {
            pane,
            logical_width,
            logical_height,
            pixel_width,
            pixel_height,
            scale_120,
            resize_epoch,
            recover_through_epoch,
        } => {
            ensure!(
                *pane != 0
                    && *logical_width != 0
                    && *logical_height != 0
                    && *pixel_width != 0
                    && *pixel_height != 0
                    && *scale_120 != 0
                    && resize_epoch.is_none_or(managed_resize_epoch_is_valid)
                    && recover_through_epoch.is_none_or(managed_resize_epoch_is_valid)
                    && !(resize_epoch.is_some() && recover_through_epoch.is_some()),
                "managed resize request has invalid units"
            );
        }
        brood_protocol::InputEvent::WindowFocus { pane } => {
            ensure!(*pane != 0, "focused pane identity is invalid");
        }
        brood_protocol::InputEvent::KeyboardKey { .. }
        | brood_protocol::InputEvent::KeyboardReset
        | brood_protocol::InputEvent::PresentationFocus { .. }
        | brood_protocol::InputEvent::PointerLeave => {}
    }
    Ok(())
}

fn validate_backend_request(request: &brood_protocol::BackendRequest) -> Result<()> {
    ensure!(request.id != 0, "backend request identity is invalid");
    match &request.operation {
        brood_protocol::BackendOperation::Control { argv } => {
            ensure!(
                !argv.is_empty()
                    && argv.len() <= 256
                    && argv.iter().all(|argument| argument.len() <= 4_096),
                "network backend command is invalid"
            );
        }
        brood_protocol::BackendOperation::Session {
            name, directory, ..
        } => {
            ensure!(
                name.as_ref().is_none_or(|name| name.len() <= 512)
                    && directory
                        .as_ref()
                        .is_none_or(|directory| directory.len() <= 4_096),
                "network session operation metadata is oversized"
            );
        }
    }
    Ok(())
}

fn validate_catalog(catalog: &brood_protocol::CatalogSnapshot) -> Result<()> {
    ensure!(catalog.sessions.len() <= 2_048 && catalog.windows.len() <= 16_384);
    let mut sessions = HashSet::with_capacity(catalog.sessions.len());
    for session in &catalog.sessions {
        ensure!(
            sessions.insert(session.id),
            "network catalog repeats a session ID"
        );
        ensure!(
            session.name.len() <= 512
                && session
                    .working_directory
                    .as_ref()
                    .is_none_or(|path| path.len() <= 4_096)
                && session.project_id.len() <= 512
                && session.project_name.len() <= 512
                && session.project_directory.len() <= 4_096
                && session
                    .branch
                    .as_ref()
                    .is_none_or(|branch| branch.len() <= 1_024)
                && session
                    .isolation
                    .capsule
                    .as_ref()
                    .and_then(|capsule| capsule.failure.as_ref())
                    .is_none_or(|failure| failure.message.len() <= 1_024),
            "network catalog session metadata is oversized"
        );
    }
    let mut panes = HashSet::with_capacity(catalog.windows.len());
    for window in &catalog.windows {
        ensure!(
            panes.insert(window.pane),
            "network catalog repeats a pane ID"
        );
        ensure!(window.app_id.len() <= 512, "network app ID is oversized");
    }
    Ok(())
}

fn validate_scene(scene: &brood_protocol::FrameScene) -> Result<()> {
    ensure!(
        scene.backend != brood_protocol::BackendId(0)
            && scene.sequence != 0
            && !scene.extent.contains(&0),
        "network scene identity or extent is invalid"
    );
    ensure!(
        scene.windows.len() <= 16_384,
        "network scene has too many windows"
    );
    let mut panes = HashSet::with_capacity(scene.windows.len());
    let mut resources = HashSet::with_capacity(scene.windows.len());
    for window in &scene.windows {
        ensure!(window.pane != 0, "network scene has an invalid pane ID");
        ensure!(panes.insert(window.pane), "network scene repeats a pane ID");
        ensure!(
            matches!(
                window.resource,
                brood_protocol::FrameResourceRef::Network { .. }
            ),
            "network scene refers to a non-network frame resource"
        );
        ensure!(
            resources.insert(window.resource.id()),
            "network scene repeats a frame resource"
        );
        ensure!(
            window.resource.id() != 0,
            "network scene has an invalid frame resource"
        );
        ensure!(
            window.source[2] > 0
                && window.source[3] > 0
                && window
                    .resize_epoch
                    .is_none_or(managed_resize_epoch_is_valid),
            "network scene has an invalid source extent or resize epoch"
        );
    }
    Ok(())
}

const fn lane_code(lane: Lane) -> u8 {
    match lane {
        Lane::Critical => 1,
        Lane::InteractiveSession0 => 2,
        Lane::Reliable => 3,
        Lane::LatestState => 4,
        Lane::Bulk => 5,
        Lane::FrameResource => 6,
        Lane::InteractiveDaemon => 7,
        Lane::InteractiveSession1 => 8,
        Lane::InteractiveSession2 => 9,
        Lane::InteractiveSession3 => 10,
    }
}

const fn lane_priority(lane: Lane) -> i32 {
    match lane {
        Lane::Critical => 20,
        Lane::InteractiveSession0
        | Lane::InteractiveSession1
        | Lane::InteractiveSession2
        | Lane::InteractiveSession3
        | Lane::InteractiveDaemon => 15,
        Lane::Reliable => 10,
        Lane::LatestState => 5,
        Lane::Bulk => -10,
        Lane::FrameResource => -20,
    }
}

fn lane_from_code(code: u8) -> Result<Lane> {
    match code {
        1 => Ok(Lane::Critical),
        2 => Ok(Lane::InteractiveSession0),
        3 => Ok(Lane::Reliable),
        4 => Ok(Lane::LatestState),
        5 => Ok(Lane::Bulk),
        6 => Ok(Lane::FrameResource),
        7 => Ok(Lane::InteractiveDaemon),
        8 => Ok(Lane::InteractiveSession1),
        9 => Ok(Lane::InteractiveSession2),
        10 => Ok(Lane::InteractiveSession3),
        _ => bail!("peer opened an unknown presentation lane"),
    }
}

const fn client_message_lane(message: &ClientMessage) -> Lane {
    match message {
        ClientMessage::Input(input) => input.lane(),
        ClientMessage::Lifecycle(_) => Lane::Critical,
        ClientMessage::Artifact(_) => Lane::Bulk,
        ClientMessage::Actor(message) => actor_message_lane(message.realm),
        ClientMessage::BackendRequest(_) | ClientMessage::ReleaseResource { .. } => Lane::Reliable,
    }
}

const fn server_message_lane(message: &ServerMessage) -> Lane {
    match message {
        ServerMessage::Actor(message) => actor_message_lane(message.realm),
        ServerMessage::Catalog(_) | ServerMessage::BackendResponse(_) => Lane::Reliable,
        ServerMessage::Scene(_) => Lane::LatestState,
        ServerMessage::Artifact(_) => Lane::Bulk,
        ServerMessage::FocusPresentation
        | ServerMessage::ClosePresentation
        | ServerMessage::ManagedResizeStatus(_)
        | ServerMessage::DisconnectAcknowledged => Lane::Critical,
    }
}

const fn actor_message_lane(realm: BackendActorRealm) -> Lane {
    match realm {
        BackendActorRealm::Daemon => Lane::InteractiveDaemon,
        BackendActorRealm::Session(session) => match session % 4 {
            0 => Lane::InteractiveSession0,
            1 => Lane::InteractiveSession1,
            2 => Lane::InteractiveSession2,
            _ => Lane::InteractiveSession3,
        },
    }
}

/// Baseline lossless remote managed-window frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkFramePacket {
    pub resource: u64,
    pub sequence: u64,
    pub pane: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: FramePixelFormat,
    pub color_space: FrameColorSpace,
    pub damage: Vec<[u32; 4]>,
    pub compressed_pixels: Vec<u8>,
}

impl NetworkFramePacket {
    /// Compress one complete packed image using the baseline lossless codec.
    #[allow(
        clippy::too_many_arguments,
        reason = "frame identity and immutable image metadata are all required"
    )]
    pub fn from_pixels(
        resource: u64,
        sequence: u64,
        pane: u64,
        width: u32,
        height: u32,
        pixel_format: FramePixelFormat,
        color_space: FrameColorSpace,
        pixels: &[u8],
    ) -> Result<Self> {
        let stride = width
            .checked_mul(4)
            .context("network frame stride overflow")?;
        let expected = usize::try_from(
            u64::from(stride)
                .checked_mul(u64::from(height))
                .context("network frame length overflow")?,
        )
        .context("network frame length exceeds address space")?;
        ensure!(
            pixels.len() == expected,
            "network frame pixel length mismatch"
        );
        ensure!(
            u64::try_from(expected)
                .ok()
                .is_some_and(|bytes| bytes <= MAX_NETWORK_FRAME_BYTES),
            "network frame exceeds the supported decompressed size"
        );
        let compressed_pixels =
            zstd::bulk::compress(pixels, 1).context("compress network frame")?;
        Ok(Self {
            resource,
            sequence,
            pane,
            width,
            height,
            stride,
            pixel_format,
            color_space,
            damage: vec![[0, 0, width, height]],
            compressed_pixels,
        })
    }

    /// Decompress and validate this packet's packed pixels.
    pub fn pixels(&self) -> Result<Vec<u8>> {
        self.validate(MAX_NETWORK_FRAME_BYTES)?;
        let expected = self.decoded_len()?;
        let pixels = zstd::bulk::decompress(&self.compressed_pixels, expected)
            .context("decompress network frame")?;
        ensure!(
            pixels.len() == expected,
            "decompressed network frame is truncated"
        );
        Ok(pixels)
    }

    /// Number of bytes occupied after lossless decompression.
    pub fn decoded_len(&self) -> Result<usize> {
        usize::try_from(
            u64::from(self.stride)
                .checked_mul(u64::from(self.height))
                .context("network frame length overflow")?,
        )
        .context("network frame length exceeds address space")
    }

    fn validate(&self, max_frame_bytes: u64) -> Result<()> {
        ensure!(
            self.resource != 0 && self.sequence != 0 && self.pane != 0,
            "network frame identity is invalid"
        );
        ensure!(
            self.width != 0 && self.height != 0,
            "network frame has an empty extent"
        );
        ensure!(
            self.stride
                == self
                    .width
                    .checked_mul(4)
                    .context("network frame stride overflow")?,
            "network frame stride does not match its pixel format"
        );
        let expected = u64::from(self.stride)
            .checked_mul(u64::from(self.height))
            .context("network frame length overflow")?;
        ensure!(
            expected <= max_frame_bytes,
            "decompressed network frame exceeds the negotiated limit"
        );
        let compressed_limit = usize::try_from(max_frame_bytes)
            .context("network frame limit exceeds address space")?
            .checked_add(FRAME_ENVELOPE_BYTES)
            .context("network compressed-frame limit overflow")?;
        ensure!(
            self.compressed_pixels.len() <= compressed_limit,
            "compressed network frame exceeds the negotiated limit"
        );
        ensure!(
            self.damage.len() <= MAX_FRAME_DAMAGE_RECTS,
            "network frame has too many damage rectangles"
        );
        for [x, y, width, height] in &self.damage {
            ensure!(
                *width != 0
                    && *height != 0
                    && x.checked_add(*width)
                        .is_some_and(|right| right <= self.width)
                    && y.checked_add(*height)
                        .is_some_and(|bottom| bottom <= self.height),
                "network frame damage rectangle is outside the image"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unreachable,
    reason = "test infrastructure failures cannot produce a useful fallback value"
)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        time::Duration,
    };

    use brood_protocol::{
        ARTIFACT_ABI_VERSION, ActorMessage, ArtifactKind, ArtifactManifest, ArtifactMessage,
        ArtifactRealm, BackendActorRealm, BackendId, BackendOperation, BackendRequest,
        BackendResponse, BackendResponseValue, CatalogSession, CatalogSnapshot, CatalogWindow,
        ClientMessage, EndpointCapabilities, FrameColorSpace, FramePixelFormat, FrameResourceRef,
        FrameScene, InputEvent, Lane, MAX_ACTOR_MESSAGE_BYTES, MAX_ARTIFACT_CHUNK_BYTES,
        MAX_MANAGED_RESIZE_EPOCH, ManagedResizeStatus, ManagedResizeStatusResult,
        ManagedWindowAutomationState, ManagedWindowSizeAuthority, PanePosition, ServerMessage,
        SessionIsolationStatus, WindowFrame,
    };
    use quinn::Endpoint;
    use rcgen::{CertifiedKey, generate_simple_self_signed};

    use super::{
        NetworkFramePacket, accept_network_connection, checked_frame_length, client_endpoint,
        client_message_lane, connect_network, lane_priority, server_endpoint,
        validate_capabilities, validate_client_message, validate_server_message,
    };

    #[test]
    fn semantic_lane_priorities_protect_interactive_traffic() {
        for interactive in [
            Lane::InteractiveSession0,
            Lane::InteractiveSession1,
            Lane::InteractiveSession2,
            Lane::InteractiveSession3,
            Lane::InteractiveDaemon,
        ] {
            assert!(lane_priority(Lane::Critical) > lane_priority(interactive));
            assert!(lane_priority(interactive) > lane_priority(Lane::Reliable));
        }
        assert!(lane_priority(Lane::Reliable) > lane_priority(Lane::LatestState));
        assert!(lane_priority(Lane::LatestState) > lane_priority(Lane::Bulk));
        assert!(lane_priority(Lane::Bulk) > lane_priority(Lane::FrameResource));
    }

    #[test]
    fn actor_mailboxes_are_isolated_by_realm_and_session_shard() {
        let message = |realm| {
            ClientMessage::Actor(ActorMessage {
                realm,
                sender: "terminal-ui".to_string(),
                recipient: "terminal-session".to_string(),
                body: vec![1],
            })
        };
        assert_eq!(
            client_message_lane(&message(BackendActorRealm::Daemon)),
            Lane::InteractiveDaemon
        );
        assert_eq!(
            client_message_lane(&message(BackendActorRealm::Session(4))),
            Lane::InteractiveSession0
        );
        assert_eq!(
            client_message_lane(&message(BackendActorRealm::Session(5))),
            Lane::InteractiveSession1
        );
        assert_eq!(
            client_message_lane(&message(BackendActorRealm::Session(6))),
            Lane::InteractiveSession2
        );
        assert_eq!(
            client_message_lane(&message(BackendActorRealm::Session(7))),
            Lane::InteractiveSession3
        );
    }

    #[test]
    fn semantic_validation_rejects_oversized_actor_messages() {
        let message = ClientMessage::Actor(ActorMessage {
            realm: BackendActorRealm::Daemon,
            sender: "sender".to_string(),
            recipient: "recipient".to_string(),
            body: vec![0; MAX_ACTOR_MESSAGE_BYTES as usize + 1],
        });
        assert!(validate_client_message(&message).is_err());
    }

    #[test]
    fn catalog_with_absent_isolation_details_round_trips_on_the_binary_carrier() {
        let message = ServerMessage::Catalog(CatalogSnapshot {
            backend: BackendId(1),
            revision: 2,
            active_session: 3,
            sessions: vec![CatalogSession {
                id: 3,
                name: "session".to_string(),
                active: true,
                windows: 1,
                working_directory: Some("/workspace".to_string()),
                project_id: "project".to_string(),
                project_name: "Project".to_string(),
                project_directory: "/workspace".to_string(),
                branch: None,
                worktree: false,
                git: true,
                isolation: SessionIsolationStatus::default(),
            }],
            windows: vec![CatalogWindow {
                pane: 4,
                session: 3,
                app_id: "terminal".to_string(),
                size_authority: ManagedWindowSizeAuthority::Presentation,
                automation: ManagedWindowAutomationState::default(),
            }],
        });
        let encoded = postcard::to_stdvec(&message)
            .unwrap_or_else(|error| unreachable!("encode catalog: {error}"));
        let decoded = postcard::from_bytes::<ServerMessage>(&encoded)
            .unwrap_or_else(|error| unreachable!("decode catalog: {error}"));
        assert_eq!(decoded, message);
    }

    #[test]
    fn frame_validation_rejects_metadata_that_cannot_match_pixels() {
        let packet = NetworkFramePacket {
            resource: 1,
            sequence: 2,
            pane: 3,
            width: 4,
            height: 4,
            stride: 15,
            pixel_format: FramePixelFormat::Rgba8,
            color_space: FrameColorSpace::Srgb,
            damage: vec![[0, 0, 4, 4]],
            compressed_pixels: vec![0],
        };
        assert!(packet.pixels().is_err());
    }

    #[test]
    fn frame_validation_rejects_zero_generation_local_identity() {
        let mut packet = NetworkFramePacket::from_pixels(
            1,
            2,
            3,
            1,
            1,
            FramePixelFormat::Rgba8,
            FrameColorSpace::Srgb,
            &[0; 4],
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        packet.resource = 0;
        assert!(packet.pixels().is_err());
    }

    #[test]
    fn endpoint_capabilities_cannot_advertise_unbounded_frames() {
        let mut capabilities = EndpointCapabilities::remote();
        capabilities.max_frame_bytes = u64::MAX;
        assert!(validate_capabilities(&capabilities).is_err());
    }

    #[test]
    fn oversized_wire_prefix_is_rejected_before_buffer_growth() {
        assert!(checked_frame_length(u32::MAX, MAX_ACTOR_MESSAGE_BYTES as usize).is_err());
    }

    #[test]
    fn managed_resize_validation_rejects_zero_units_and_epochs() {
        let invalid_input = ClientMessage::Input(InputEvent::WindowResize {
            pane: 7,
            logical_width: 200,
            logical_height: 120,
            pixel_width: 300,
            pixel_height: 180,
            scale_120: 180,
            resize_epoch: Some(0),
            recover_through_epoch: None,
        });
        assert!(validate_client_message(&invalid_input).is_err());

        for (resize_epoch, recover_through_epoch) in [
            (None, Some(0)),
            (Some(41), Some(40)),
            (Some(MAX_MANAGED_RESIZE_EPOCH + 1), None),
            (None, Some(MAX_MANAGED_RESIZE_EPOCH + 1)),
        ] {
            let invalid_recovery = ClientMessage::Input(InputEvent::WindowResize {
                pane: 7,
                logical_width: 200,
                logical_height: 120,
                pixel_width: 300,
                pixel_height: 180,
                scale_120: 180,
                resize_epoch,
                recover_through_epoch,
            });
            assert!(validate_client_message(&invalid_recovery).is_err());
        }

        let invalid_status = ServerMessage::ManagedResizeStatus(ManagedResizeStatus {
            pane: 7,
            resize_epoch: 41,
            result: ManagedResizeStatusResult::Configured {
                logical_size: [200, 0],
                pixel_size: [300, 180],
                scale_120: 180,
            },
        });
        assert!(validate_server_message(&invalid_status).is_err());

        let invalid_scene = ServerMessage::Scene(FrameScene {
            backend: brood_protocol::BackendId(7),
            sequence: 2,
            extent: [1_280, 800],
            windows: vec![WindowFrame {
                pane: 7,
                resource: FrameResourceRef::Network { resource: 1 },
                source: [0, 0, 300, 180],
                active: true,
                resize_epoch: Some(0),
            }],
        });
        assert!(validate_server_message(&invalid_scene).is_err());

        let invalid_status = ServerMessage::ManagedResizeStatus(ManagedResizeStatus {
            pane: 7,
            resize_epoch: MAX_MANAGED_RESIZE_EPOCH + 1,
            result: ManagedResizeStatusResult::Rejected {
                reason: brood_protocol::ManagedResizeRejectionReason::ConfigureFailed,
            },
        });
        assert!(validate_server_message(&invalid_status).is_err());

        let invalid_scene = ServerMessage::Scene(FrameScene {
            backend: brood_protocol::BackendId(7),
            sequence: 2,
            extent: [1_280, 800],
            windows: vec![WindowFrame {
                pane: 7,
                resource: FrameResourceRef::Network { resource: 1 },
                source: [0, 0, 300, 180],
                active: true,
                resize_epoch: Some(MAX_MANAGED_RESIZE_EPOCH + 1),
            }],
        });
        assert!(validate_server_message(&invalid_scene).is_err());
    }

    fn endpoints() -> (Endpoint, Endpoint, SocketAddr) {
        let CertifiedKey {
            cert: server_cert,
            signing_key: server_key,
        } = generate_simple_self_signed(vec!["localhost".into()])
            .unwrap_or_else(|error| unreachable!("{error}"));
        let CertifiedKey {
            cert: client_cert,
            signing_key: client_key,
        } = generate_simple_self_signed(vec!["brood-client".into()])
            .unwrap_or_else(|error| unreachable!("{error}"));
        let server = server_endpoint(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            server_cert.der().to_vec(),
            server_key.serialize_der(),
            client_cert.der().to_vec(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let address = server
            .local_addr()
            .unwrap_or_else(|error| unreachable!("{error}"));
        let client = client_endpoint(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            server_cert.der().to_vec(),
            client_cert.der().to_vec(),
            client_key.serialize_der(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        (server, client, address)
    }

    #[tokio::test]
    async fn quic_carrier_preserves_semantic_lanes_and_frames() {
        let (server_endpoint, client_endpoint, address) = endpoints();
        let server_task = tokio::spawn(async move {
            let incoming = server_endpoint
                .accept()
                .await
                .unwrap_or_else(|| unreachable!("server endpoint closed"));
            let mut connection =
                accept_network_connection(incoming, EndpointCapabilities::remote(), 42)
                    .await
                    .unwrap_or_else(|error| unreachable!("{error}"));
            let input = connection
                .receive_client_message(Lane::LatestState)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            assert!(matches!(
                input,
                ClientMessage::Input(InputEvent::PointerMotion {
                    position: PanePosition { pane: 7, .. }
                })
            ));
            let backend = connection
                .receive_client_message(Lane::Reliable)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            assert_eq!(
                backend,
                ClientMessage::BackendRequest(BackendRequest {
                    id: 9,
                    operation: BackendOperation::Control {
                        argv: vec!["status".to_string()],
                    },
                })
            );
            let artifact = connection
                .receive_client_message(Lane::Bulk)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            let ClientMessage::Artifact(ArtifactMessage::Offer(manifest)) = artifact else {
                unreachable!("bulk lane did not preserve the artifact offer");
            };
            let chunk = connection
                .receive_client_message(Lane::Bulk)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            assert_eq!(
                chunk,
                ClientMessage::Artifact(ArtifactMessage::Chunk {
                    digest: manifest.digest,
                    offset: 0,
                    bytes: vec![0x70; MAX_ARTIFACT_CHUNK_BYTES],
                })
            );
            connection
                .send_server_message(&ServerMessage::Artifact(ArtifactMessage::Accepted {
                    digest: manifest.digest,
                }))
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_server_message(&ServerMessage::BackendResponse(BackendResponse {
                    id: 9,
                    result: Ok(BackendResponseValue::Control("ready".to_string())),
                }))
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_server_message(&ServerMessage::ManagedResizeStatus(ManagedResizeStatus {
                    pane: 7,
                    resize_epoch: 41,
                    result: ManagedResizeStatusResult::Configured {
                        logical_size: [200, 120],
                        pixel_size: [300, 180],
                        scale_120: 180,
                    },
                }))
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_server_message(&ServerMessage::FocusPresentation)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_server_message(&ServerMessage::Scene(FrameScene {
                    backend: brood_protocol::BackendId(7),
                    sequence: 2,
                    extent: [1_280, 800],
                    windows: vec![WindowFrame {
                        pane: 7,
                        resource: FrameResourceRef::Network { resource: 1 },
                        source: [0, 0, 300, 180],
                        active: true,
                        resize_epoch: Some(41),
                    }],
                }))
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            let frame = NetworkFramePacket::from_pixels(
                1,
                2,
                7,
                2,
                2,
                FramePixelFormat::Rgba8,
                FrameColorSpace::Srgb,
                &[0x44; 16],
            )
            .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_frame(&frame)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            let resize = connection
                .receive_client_message(Lane::Critical)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            assert_eq!(
                resize,
                ClientMessage::Input(InputEvent::WindowResize {
                    pane: 7,
                    logical_width: 200,
                    logical_height: 120,
                    pixel_width: 300,
                    pixel_height: 180,
                    scale_120: 180,
                    resize_epoch: Some(41),
                    recover_through_epoch: None,
                })
            );
            let disconnect = connection
                .receive_client_message(Lane::Critical)
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            assert_eq!(
                disconnect,
                ClientMessage::Lifecycle(brood_protocol::ClientLifecycle::Disconnect)
            );
        });

        let mut client = connect_network(
            &client_endpoint,
            address,
            "localhost",
            brood_protocol::BackendId(7),
            EndpointCapabilities::remote(),
        )
        .await
        .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(client.connection_generation, 42);
        client
            .send_client_message(&ClientMessage::Input(InputEvent::PointerMotion {
                position: PanePosition {
                    pane: 7,
                    x: 1.5,
                    y: 2.5,
                },
            }))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        client
            .send_client_message(&ClientMessage::BackendRequest(BackendRequest {
                id: 9,
                operation: BackendOperation::Control {
                    argv: vec!["status".to_string()],
                },
            }))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        client
            .send_client_message(&ClientMessage::Artifact(ArtifactMessage::Offer(
                ArtifactManifest {
                    id: "workspace.ui".to_string(),
                    realm: ArtifactRealm::Session,
                    kind: ArtifactKind::WasmActor,
                    digest: [9; 32],
                    bytes: 42,
                    abi_version: ARTIFACT_ABI_VERSION,
                    capabilities: Vec::new(),
                    package: None,
                },
            )))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        client
            .send_client_message(&ClientMessage::Artifact(ArtifactMessage::Chunk {
                digest: [9; 32],
                offset: 0,
                bytes: vec![0x70; MAX_ARTIFACT_CHUNK_BYTES],
            }))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            client
                .receive_server_message(Lane::Reliable)
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ServerMessage::BackendResponse(BackendResponse {
                id: 9,
                result: Ok(BackendResponseValue::Control("ready".to_string())),
            })
        );
        assert_eq!(
            client
                .receive_server_message(Lane::Critical)
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ServerMessage::ManagedResizeStatus(ManagedResizeStatus {
                pane: 7,
                resize_epoch: 41,
                result: ManagedResizeStatusResult::Configured {
                    logical_size: [200, 120],
                    pixel_size: [300, 180],
                    scale_120: 180,
                },
            })
        );
        assert_eq!(
            client
                .receive_server_message(Lane::Critical)
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ServerMessage::FocusPresentation
        );
        assert_eq!(
            client
                .receive_server_message(Lane::Bulk)
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ServerMessage::Artifact(ArtifactMessage::Accepted { digest: [9; 32] })
        );
        let frame = client
            .receive_frame()
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            frame
                .pixels()
                .unwrap_or_else(|error| unreachable!("{error}")),
            vec![0x44; 16]
        );
        assert_eq!(
            client
                .receive_server_message(Lane::LatestState)
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ServerMessage::Scene(FrameScene {
                backend: brood_protocol::BackendId(7),
                sequence: 2,
                extent: [1_280, 800],
                windows: vec![WindowFrame {
                    pane: 7,
                    resource: FrameResourceRef::Network { resource: 1 },
                    source: [0, 0, 300, 180],
                    active: true,
                    resize_epoch: Some(41),
                }],
            })
        );
        client
            .send_client_message(&ClientMessage::Input(InputEvent::WindowResize {
                pane: 7,
                logical_width: 200,
                logical_height: 120,
                pixel_width: 300,
                pixel_height: 180,
                scale_120: 180,
                resize_epoch: Some(41),
                recover_through_epoch: None,
            }))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        client
            .send_client_message(&ClientMessage::Lifecycle(
                brood_protocol::ClientLifecycle::Disconnect,
            ))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        server_task
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
    }

    #[tokio::test]
    async fn stalled_frame_stream_does_not_block_critical_input() {
        let (server_endpoint, client_endpoint, address) = endpoints();
        let server_task = tokio::spawn(async move {
            let incoming = server_endpoint
                .accept()
                .await
                .unwrap_or_else(|| unreachable!("server endpoint closed"));
            let mut connection =
                accept_network_connection(incoming, EndpointCapabilities::remote(), 42)
                    .await
                    .unwrap_or_else(|error| unreachable!("{error}"));
            let (_critical_send, mut critical_receive) = connection
                .take_lane(Lane::Critical)
                .unwrap_or_else(|error| unreachable!("{error}"))
                .split();
            let (_frame_send, _frame_receive) = connection
                .take_lane(Lane::FrameResource)
                .unwrap_or_else(|error| unreachable!("{error}"))
                .split();
            tokio::time::timeout(
                Duration::from_secs(2),
                critical_receive.receive_client_message(),
            )
            .await
            .unwrap_or_else(|error| unreachable!("critical lane timed out: {error}"))
            .unwrap_or_else(|error| unreachable!("{error}"))
        });

        let mut client = connect_network(
            &client_endpoint,
            address,
            "localhost",
            brood_protocol::BackendId(7),
            EndpointCapabilities::remote(),
        )
        .await
        .unwrap_or_else(|error| unreachable!("{error}"));
        let (mut critical_send, _critical_receive) = client
            .take_lane(Lane::Critical)
            .unwrap_or_else(|error| unreachable!("{error}"))
            .split();
        let (mut frame_send, _frame_receive) = client
            .take_lane(Lane::FrameResource)
            .unwrap_or_else(|error| unreachable!("{error}"))
            .split();
        let stalled_frame = tokio::spawn(async move {
            let packet = NetworkFramePacket {
                resource: 1,
                sequence: 1,
                pane: 1,
                width: 2_048,
                height: 1_024,
                stride: 8_192,
                pixel_format: FramePixelFormat::Rgba8,
                color_space: FrameColorSpace::Srgb,
                damage: vec![[0, 0, 2_048, 1_024]],
                compressed_pixels: vec![0xa5; 8 * 1_024 * 1_024],
            };
            frame_send.send_frame(&packet).await
        });
        tokio::task::yield_now().await;
        critical_send
            .send_client_message(&ClientMessage::Input(InputEvent::KeyboardReset))
            .await
            .unwrap_or_else(|error| unreachable!("{error}"));
        assert_eq!(
            server_task
                .await
                .unwrap_or_else(|error| unreachable!("{error}")),
            ClientMessage::Input(InputEvent::KeyboardReset)
        );
        stalled_frame.abort();
    }

    #[tokio::test]
    async fn quic_carrier_rejects_an_untrusted_client_certificate() {
        let CertifiedKey {
            cert: server_cert,
            signing_key: server_key,
        } = generate_simple_self_signed(vec!["localhost".into()])
            .unwrap_or_else(|error| unreachable!("{error}"));
        let CertifiedKey {
            cert: trusted_client_cert,
            ..
        } = generate_simple_self_signed(vec!["trusted-client".into()])
            .unwrap_or_else(|error| unreachable!("{error}"));
        let CertifiedKey {
            cert: untrusted_client_cert,
            signing_key: untrusted_client_key,
        } = generate_simple_self_signed(vec!["untrusted-client".into()])
            .unwrap_or_else(|error| unreachable!("{error}"));
        let server = server_endpoint(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            server_cert.der().to_vec(),
            server_key.serialize_der(),
            trusted_client_cert.der().to_vec(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let address = server
            .local_addr()
            .unwrap_or_else(|error| unreachable!("{error}"));
        let client = client_endpoint(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            server_cert.der().to_vec(),
            untrusted_client_cert.der().to_vec(),
            untrusted_client_key.serialize_der(),
        )
        .unwrap_or_else(|error| unreachable!("{error}"));
        let server_task = tokio::spawn(async move {
            let incoming = server
                .accept()
                .await
                .unwrap_or_else(|| unreachable!("server endpoint closed"));
            accept_network_connection(incoming, EndpointCapabilities::remote(), 42).await
        });

        assert!(
            connect_network(
                &client,
                address,
                "localhost",
                brood_protocol::BackendId(7),
                EndpointCapabilities::remote(),
            )
            .await
            .is_err()
        );
        assert!(
            server_task
                .await
                .unwrap_or_else(|error| unreachable!("{error}"))
                .is_err()
        );
    }
}
