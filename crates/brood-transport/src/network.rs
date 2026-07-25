use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use brood_protocol::{
    CarrierKind, ClientMessage, EndpointCapabilities, FrameColorSpace, FramePixelFormat, Lane,
    ServerMessage, TransportProfile, negotiate_profile,
};
use quinn::{
    Connection, Endpoint, Incoming, RecvStream, SendStream,
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
    let tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![certificate], private_key)
        .context("configure QUIC server identity")?;
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
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![rustls::pki_types::CertificateDer::from(certificate_der)],
            rustls::pki_types::PrivatePkcs8KeyDer::from(private_key_der).into(),
        )
        .context("configure QUIC presentation client identity")?;
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
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    config
}

const LANES: [Lane; 5] = [
    Lane::Critical,
    Lane::Reliable,
    Lane::LatestState,
    Lane::Bulk,
    Lane::FrameResource,
];
const MAX_WIRE_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// First authenticated application message sent by the remote client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    pub capabilities: EndpointCapabilities,
}

/// Server-selected immutable profile and connection generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerHello {
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
}

/// Sending half of one semantic QUIC lane.
pub struct NetworkSendLane {
    _connection: Connection,
    lane: Lane,
    send: SendStream,
}

/// Receiving half of one semantic QUIC lane.
pub struct NetworkReceiveLane {
    _connection: Connection,
    lane: Lane,
    receive: RecvStream,
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
            },
            NetworkReceiveLane {
                _connection: self.connection,
                lane: self.lane,
                receive: self.receive,
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
        self.send_value(message).await
    }

    /// Receive and validate a client message.
    pub async fn receive_client_message(&mut self) -> Result<ClientMessage> {
        let message = self.receive_value().await?;
        ensure!(
            client_message_lane(&message) == self.lane,
            "client message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Send a server message after validating its semantic lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        ensure!(
            server_message_lane(message) == self.lane,
            "server message belongs on a different semantic lane"
        );
        self.send_value(message).await
    }

    /// Receive and validate a server message.
    pub async fn receive_server_message(&mut self) -> Result<ServerMessage> {
        let message = self.receive_value().await?;
        ensure!(
            server_message_lane(&message) == self.lane,
            "server message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Send a compressed frame on the frame-resource lane.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        self.send_value(frame).await
    }

    /// Receive a compressed frame from the frame-resource lane.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        self.receive_value().await
    }

    async fn send_value<T: Serialize + Sync>(&mut self, value: &T) -> Result<()> {
        let bytes = postcard::to_stdvec(value).context("encode presentation network message")?;
        ensure!(
            bytes.len() <= MAX_WIRE_MESSAGE_BYTES,
            "presentation network message exceeds the carrier limit"
        );
        write_frame(&mut self.send, &bytes).await
    }

    async fn receive_value<T: DeserializeOwned>(&mut self) -> Result<T> {
        let bytes = read_frame(&mut self.receive).await?;
        postcard::from_bytes(&bytes).context("decode presentation network message")
    }
}

impl NetworkSendLane {
    /// Send a client message after validating its semantic lane.
    pub async fn send_client_message(&mut self, message: &ClientMessage) -> Result<()> {
        ensure!(
            client_message_lane(message) == self.lane,
            "client message belongs on a different semantic lane"
        );
        send_value(&mut self.send, message).await
    }

    /// Send a server message after validating its semantic lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        ensure!(
            server_message_lane(message) == self.lane,
            "server message belongs on a different semantic lane"
        );
        send_value(&mut self.send, message).await
    }

    /// Send a compressed frame on the frame-resource lane.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        send_value(&mut self.send, frame).await
    }

    /// Finish this lane and wait until the peer acknowledges every buffered byte.
    pub async fn finish(&mut self) -> Result<()> {
        self.send
            .finish()
            .context("finish presentation network lane")?;
        let _peer_stop = self
            .send
            .stopped()
            .await
            .context("wait for presentation network lane acknowledgement")?;
        Ok(())
    }
}

impl NetworkReceiveLane {
    /// Receive and validate a client message.
    pub async fn receive_client_message(&mut self) -> Result<ClientMessage> {
        let message = receive_value(&mut self.receive).await?;
        ensure!(
            client_message_lane(&message) == self.lane,
            "client message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Receive and validate a server message.
    pub async fn receive_server_message(&mut self) -> Result<ServerMessage> {
        let message = receive_value(&mut self.receive).await?;
        ensure!(
            server_message_lane(&message) == self.lane,
            "server message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Receive a compressed frame from the frame-resource lane.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        ensure!(
            self.lane == Lane::FrameResource,
            "network frame belongs on the frame-resource lane"
        );
        receive_value(&mut self.receive).await
    }

    /// Read the peer's lane finish marker after its final framed message.
    pub async fn wait_for_finish(&mut self) -> Result<()> {
        let trailing = self
            .receive
            .read_to_end(0)
            .await
            .context("wait for presentation network lane finish")?;
        ensure!(
            trailing.is_empty(),
            "peer sent trailing bytes after the final presentation message"
        );
        Ok(())
    }
}

async fn send_value<T: Serialize + Sync>(send: &mut SendStream, value: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(value).context("encode presentation network message")?;
    ensure!(
        bytes.len() <= MAX_WIRE_MESSAGE_BYTES,
        "presentation network message exceeds the carrier limit"
    );
    write_frame(send, &bytes).await
}

async fn receive_value<T: DeserializeOwned>(receive: &mut RecvStream) -> Result<T> {
    let bytes = read_frame(receive).await?;
    postcard::from_bytes(&bytes).context("decode presentation network message")
}

/// One mutually authenticated QUIC presentation carrier.
///
/// Each semantic traffic class owns a distinct ordered QUIC stream, preventing
/// frame congestion from causing head-of-line blocking for critical input.
pub struct NetworkConnection {
    _connection: Connection,
    lanes: HashMap<Lane, NetworkLane>,
    pub profile: TransportProfile,
    pub connection_generation: u64,
    pub peer_capabilities: EndpointCapabilities,
}

impl NetworkConnection {
    /// Detach one semantic lane for an independent I/O task.
    pub fn take_lane(&mut self, lane: Lane) -> Result<NetworkLane> {
        self.lanes
            .remove(&lane)
            .context("presentation network lane is unavailable")
    }

    /// Send one client message on its required logical lane.
    pub async fn send_client_message(&mut self, message: &ClientMessage) -> Result<()> {
        self.send(client_message_lane(message), message).await
    }

    /// Receive one client message from a specified logical lane.
    pub async fn receive_client_message(&mut self, lane: Lane) -> Result<ClientMessage> {
        let message = self.receive(lane).await?;
        ensure!(
            client_message_lane(&message) == lane,
            "client message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Send one server message on its required logical lane.
    pub async fn send_server_message(&mut self, message: &ServerMessage) -> Result<()> {
        self.send(server_message_lane(message), message).await
    }

    /// Receive one server message from a specified logical lane.
    pub async fn receive_server_message(&mut self, lane: Lane) -> Result<ServerMessage> {
        let message = self.receive(lane).await?;
        ensure!(
            server_message_lane(&message) == lane,
            "server message arrived on the wrong semantic lane"
        );
        Ok(message)
    }

    /// Send one compressed managed-window frame packet.
    pub async fn send_frame(&mut self, frame: &NetworkFramePacket) -> Result<()> {
        self.send(Lane::FrameResource, frame).await
    }

    /// Receive one compressed managed-window frame packet.
    pub async fn receive_frame(&mut self) -> Result<NetworkFramePacket> {
        self.receive(Lane::FrameResource).await
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
    capabilities: EndpointCapabilities,
) -> Result<NetworkConnection> {
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
            capabilities: capabilities.clone(),
        },
    )
    .await?;
    let hello: ServerHello = read_serialized(&mut handshake_receive).await?;
    ensure!(
        hello.profile == TransportProfile::REMOTE,
        "server selected a non-network profile for a remote connection"
    );
    let lanes = open_lanes(&connection).await?;
    Ok(NetworkConnection {
        _connection: connection,
        lanes,
        profile: hello.profile,
        connection_generation: hello.connection_generation,
        peer_capabilities: hello.capabilities,
    })
}

/// Accept one mutually authenticated remote presentation client.
pub async fn accept_network_connection(
    incoming: Incoming,
    capabilities: EndpointCapabilities,
    connection_generation: u64,
) -> Result<NetworkConnection> {
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
    let profile = negotiate_profile(&hello.capabilities, &capabilities)
        .context("negotiate remote presentation profile")?;
    write_serialized(
        &mut handshake_send,
        &ServerHello {
            profile,
            connection_generation,
            capabilities: capabilities.clone(),
        },
    )
    .await?;
    let lanes = accept_lanes(&connection).await?;
    Ok(NetworkConnection {
        _connection: connection,
        lanes,
        profile,
        connection_generation,
        peer_capabilities: hello.capabilities,
    })
}

async fn open_lanes(connection: &Connection) -> Result<HashMap<Lane, NetworkLane>> {
    let mut lanes = HashMap::with_capacity(LANES.len());
    for lane in LANES {
        let (mut send, receive) = connection
            .open_bi()
            .await
            .with_context(|| format!("open {lane:?} presentation stream"))?;
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
            },
        );
    }
    Ok(lanes)
}

async fn accept_lanes(connection: &Connection) -> Result<HashMap<Lane, NetworkLane>> {
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
        ensure!(
            lanes
                .insert(
                    lane,
                    NetworkLane {
                        connection: connection.clone(),
                        lane,
                        send,
                        receive,
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
    write_frame(stream, &bytes).await
}

async fn read_serialized<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T> {
    let bytes = read_frame(stream).await?;
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

async fn read_frame(stream: &mut RecvStream) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .await
        .context("read presentation message length")?;
    let length = usize::try_from(u32::from_be_bytes(length))
        .context("presentation message length is invalid")?;
    ensure!(
        length <= MAX_WIRE_MESSAGE_BYTES,
        "presentation message length {length} exceeds the carrier limit of \
         {MAX_WIRE_MESSAGE_BYTES} bytes"
    );
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .context("read presentation message body")?;
    Ok(bytes)
}

const fn lane_code(lane: Lane) -> u8 {
    match lane {
        Lane::Critical => 1,
        Lane::Reliable => 2,
        Lane::LatestState => 3,
        Lane::Bulk => 4,
        Lane::FrameResource => 5,
    }
}

fn lane_from_code(code: u8) -> Result<Lane> {
    match code {
        1 => Ok(Lane::Critical),
        2 => Ok(Lane::Reliable),
        3 => Ok(Lane::LatestState),
        4 => Ok(Lane::Bulk),
        5 => Ok(Lane::FrameResource),
        _ => bail!("peer opened an unknown presentation lane"),
    }
}

const fn client_message_lane(message: &ClientMessage) -> Lane {
    match message {
        ClientMessage::Input(input) => input.lane(),
        ClientMessage::Lifecycle(_) => Lane::Critical,
        ClientMessage::Actor(_)
        | ClientMessage::BackendRequest(_)
        | ClientMessage::ReleaseResource { .. } => Lane::Reliable,
    }
}

const fn server_message_lane(message: &ServerMessage) -> Lane {
    match message {
        ServerMessage::Catalog(_) | ServerMessage::Actor(_) | ServerMessage::BackendResponse(_) => {
            Lane::Reliable
        }
        ServerMessage::Scene(_) => Lane::LatestState,
        ServerMessage::FocusPresentation
        | ServerMessage::ClosePresentation
        | ServerMessage::DisconnectAcknowledged => Lane::Critical,
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
        let expected = usize::try_from(
            u64::from(self.stride)
                .checked_mul(u64::from(self.height))
                .context("network frame length overflow")?,
        )
        .context("network frame length exceeds address space")?;
        ensure!(
            expected <= MAX_WIRE_MESSAGE_BYTES,
            "decompressed network frame exceeds the carrier limit"
        );
        let pixels = zstd::bulk::decompress(&self.compressed_pixels, expected)
            .context("decompress network frame")?;
        ensure!(
            pixels.len() == expected,
            "decompressed network frame is truncated"
        );
        Ok(pixels)
    }
}

#[cfg(test)]
#[allow(
    clippy::unreachable,
    reason = "test infrastructure failures cannot produce a useful fallback value"
)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use brood_protocol::{
        BackendOperation, BackendRequest, BackendResponse, BackendResponseValue, ClientMessage,
        EndpointCapabilities, FrameColorSpace, FramePixelFormat, InputEvent, Lane, PanePosition,
        ServerMessage,
    };
    use quinn::Endpoint;
    use rcgen::{CertifiedKey, generate_simple_self_signed};

    use super::{
        NetworkFramePacket, accept_network_connection, client_endpoint, connect_network,
        server_endpoint,
    };

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
            connection
                .send_server_message(&ServerMessage::BackendResponse(BackendResponse {
                    id: 9,
                    result: Ok(BackendResponseValue::Control("ready".to_string())),
                }))
                .await
                .unwrap_or_else(|error| unreachable!("{error}"));
            connection
                .send_server_message(&ServerMessage::FocusPresentation)
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
            ServerMessage::FocusPresentation
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
