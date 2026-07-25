use std::{
    collections::{HashMap, HashSet},
    fs,
    mem::size_of,
    net::{Ipv4Addr, SocketAddr},
    os::fd::AsFd,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use brood_cli::control::{
    ControlTarget, handshake, open_presentation_fast_path, resolve_control_target,
    shared_memory_resource,
};
use brood_protocol::{
    ActorMessage, CatalogSession, CatalogSnapshot, CatalogWindow, ClientLifecycle, ClientMessage,
    EndpointCapabilities, FrameResourceRef, FrameScene, FrameTransportKind, InputEvent, Lane,
    ServerMessage, WindowFrame,
};
use brood_transport::{
    NetworkFramePacket, NetworkSendLane, accept_network_connection, client_endpoint,
    connect_network, server_endpoint,
};
use clap::{Parser, Subcommand};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Parser)]
#[command(about = "Brood remote presentation transport")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a self-signed development certificate and PKCS#8 key.
    GenerateCertificate {
        #[arg(long, default_value = "localhost")]
        server_name: String,
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
    },
    /// Relay one local session connection over authenticated QUIC.
    Relay {
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        trusted_client_certificate: PathBuf,
        #[arg(long = "dbus-destination", alias = "dbus-name")]
        dbus_name: Option<String>,
        #[arg(long = "dbus-address")]
        dbus_address: Option<String>,
    },
    /// Verify a remote catalog, real frame, and critical acknowledgement.
    Probe {
        #[arg(long)]
        connect: SocketAddr,
        #[arg(long, default_value = "localhost")]
        server_name: String,
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        client_certificate: PathBuf,
        #[arg(long)]
        client_private_key: PathBuf,
    },
}

#[derive(Clone)]
struct Outbound {
    critical: mpsc::UnboundedSender<ServerMessage>,
    reliable: mpsc::UnboundedSender<ServerMessage>,
    latest: mpsc::UnboundedSender<ServerMessage>,
    frames: mpsc::UnboundedSender<NetworkFramePacket>,
}

#[derive(Deserialize)]
struct LocalCatalog {
    active_session: u64,
    sessions: Vec<CatalogSession>,
    windows: Vec<CatalogWindow>,
}

struct PendingScene {
    sequence: u64,
    expected_windows: usize,
    extent: [u32; 2],
    windows: Vec<PendingWindow>,
}

struct PendingWindow {
    pane: u64,
    resource: u64,
    source: [i32; 4],
    active: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::GenerateCertificate {
            server_name,
            certificate,
            private_key,
        } => generate_certificate(&server_name, &certificate, &private_key),
        Command::Relay {
            listen,
            certificate,
            private_key,
            trusted_client_certificate,
            dbus_name,
            dbus_address,
        } => {
            relay(
                listen,
                &certificate,
                &private_key,
                &trusted_client_certificate,
                resolve_control_target(dbus_name, dbus_address),
            )
            .await
        }
        Command::Probe {
            connect,
            server_name,
            certificate,
            client_certificate,
            client_private_key,
        } => {
            probe(
                connect,
                &server_name,
                &certificate,
                &client_certificate,
                &client_private_key,
            )
            .await
        }
    }
}

fn generate_certificate(server_name: &str, certificate: &Path, key: &Path) -> Result<()> {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![server_name.to_string()])
            .context("generate remote presentation certificate")?;
    fs::write(certificate, cert.der()).with_context(|| {
        format!(
            "write remote presentation certificate {}",
            certificate.display()
        )
    })?;
    fs::write(key, signing_key.serialize_der())
        .with_context(|| format!("write remote presentation private key {}", key.display()))?;
    Ok(())
}

async fn relay(
    listen: SocketAddr,
    certificate: &Path,
    private_key: &Path,
    trusted_client_certificate: &Path,
    target: ControlTarget,
) -> Result<()> {
    let bootstrap_target = target.clone();
    let (handshake, fast_path) = tokio::task::spawn_blocking(move || {
        let handshake =
            handshake(&bootstrap_target).context("connect relay to local session server")?;
        let fast_path =
            open_presentation_fast_path(&bootstrap_target, &[FrameTransportKind::SharedMemory])?;
        Ok::<_, anyhow::Error>((handshake, fast_path))
    })
    .await
    .context("join local session bootstrap")??;
    ensure!(
        fast_path.profile.frames == FrameTransportKind::SharedMemory,
        "remote relay requires the local shared-memory frame profile"
    );

    let endpoint = server_endpoint(
        listen,
        fs::read(certificate)
            .with_context(|| format!("read certificate {}", certificate.display()))?,
        fs::read(private_key)
            .with_context(|| format!("read private key {}", private_key.display()))?,
        fs::read(trusted_client_certificate).with_context(|| {
            format!(
                "read trusted client certificate {}",
                trusted_client_certificate.display()
            )
        })?,
    )?;
    println!(
        "LISTEN={} protocol={} local_generation={}",
        endpoint.local_addr()?,
        handshake.protocol_version,
        fast_path.generation
    );

    let incoming = endpoint
        .accept()
        .await
        .context("remote relay listener closed")?;
    let mut network = accept_network_connection(
        incoming,
        EndpointCapabilities::remote(),
        fast_path.generation,
    )
    .await?;

    let (critical_tx, critical_rx) = mpsc::unbounded_channel();
    let (reliable_tx, reliable_rx) = mpsc::unbounded_channel();
    let (latest_tx, latest_rx) = mpsc::unbounded_channel();
    let (frame_tx, frame_rx) = mpsc::unbounded_channel();
    let outbound = Outbound {
        critical: critical_tx,
        reliable: reliable_tx,
        latest: latest_tx,
        frames: frame_tx,
    };

    spawn_catalog_forwarder(
        &fast_path.catalog_fd,
        fast_path.catalog_notify_fd,
        fast_path.generation,
        outbound.clone(),
    )?;
    spawn_frame_forwarder(
        target,
        &fast_path.frame_fd,
        fast_path.frame_notify_fd,
        fast_path.generation,
        outbound.clone(),
    )?;
    spawn_actor_forwarder(
        &fast_path.presentation_message_fd,
        fast_path.presentation_message_notify_fd,
        fast_path.generation,
        outbound,
    )?;

    let input = brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
    let input_notifications = brood_ipc::EventNotifier::from_owned_fd(fast_path.input_notify_fd);
    let actor_messages = brood_ipc::ActorMessageProducer::map(
        fast_path.actor_message_fd.as_fd(),
        fast_path.generation,
    )?;
    let actor_notifications =
        brood_ipc::EventNotifier::from_owned_fd(fast_path.actor_message_notify_fd);
    let (local_input_tx, local_input_rx) = mpsc::unbounded_channel();
    let (local_actor_tx, local_actor_rx) = mpsc::unbounded_channel();
    tokio::spawn(write_local_input(
        input,
        input_notifications,
        fast_path.generation,
        local_input_rx,
    ));
    tokio::spawn(write_local_actor_messages(
        actor_messages,
        actor_notifications,
        fast_path.generation,
        local_actor_rx,
    ));

    let (critical_send, mut critical_receive) = network.take_lane(Lane::Critical)?.split();
    let (reliable_send, reliable_receive) = network.take_lane(Lane::Reliable)?.split();
    let (latest_send, latest_receive) = network.take_lane(Lane::LatestState)?.split();
    let (frame_send, _frame_receive) = network.take_lane(Lane::FrameResource)?.split();

    tokio::spawn(send_server_messages(critical_send, critical_rx));
    tokio::spawn(send_server_messages(reliable_send, reliable_rx));
    tokio::spawn(send_server_messages(latest_send, latest_rx));
    tokio::spawn(send_network_frames(frame_send, frame_rx));

    let (disconnect_tx, disconnect_rx) = oneshot::channel();
    let critical_input_tx = local_input_tx.clone();
    tokio::spawn(async move {
        loop {
            let message = critical_receive.receive_client_message().await?;
            let disconnect = message == ClientMessage::Lifecycle(ClientLifecycle::Disconnect);
            critical_input_tx
                .send(message)
                .map_err(|_error| anyhow!("local input writer stopped"))?;
            if disconnect {
                let _send_result = disconnect_tx.send(());
                return Ok::<(), anyhow::Error>(());
            }
        }
    });
    tokio::spawn(receive_latest_input(latest_receive, local_input_tx));
    tokio::spawn(receive_reliable_input(reliable_receive, local_actor_tx));

    disconnect_rx
        .await
        .context("remote client disconnected without a lifecycle message")?;
    Ok(())
}

async fn receive_latest_input(
    mut lane: brood_transport::NetworkReceiveLane,
    sender: mpsc::UnboundedSender<ClientMessage>,
) -> Result<()> {
    loop {
        let message = lane.receive_client_message().await?;
        sender
            .send(message)
            .map_err(|_error| anyhow!("local input writer stopped"))?;
    }
}

async fn receive_reliable_input(
    mut lane: brood_transport::NetworkReceiveLane,
    sender: mpsc::UnboundedSender<ActorMessage>,
) -> Result<()> {
    loop {
        match lane.receive_client_message().await? {
            ClientMessage::Actor(message) => sender
                .send(message)
                .map_err(|_error| anyhow!("local actor writer stopped"))?,
            ClientMessage::ReleaseResource { .. } => {}
            ClientMessage::Input(_) | ClientMessage::Lifecycle(_) => {
                bail!("critical client message reached the reliable lane");
            }
        }
    }
}

async fn send_server_messages(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::UnboundedReceiver<ServerMessage>,
) -> Result<()> {
    while let Some(message) = receiver.recv().await {
        lane.send_server_message(&message).await?;
    }
    Ok(())
}

async fn send_network_frames(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::UnboundedReceiver<NetworkFramePacket>,
) -> Result<()> {
    while let Some(frame) = receiver.recv().await {
        lane.send_frame(&frame).await?;
    }
    Ok(())
}

async fn write_local_input(
    mut producer: brood_ipc::CriticalProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    mut receiver: mpsc::UnboundedReceiver<ClientMessage>,
) -> Result<()> {
    let mut sequence = 0_u64;
    while let Some(message) = receiver.recv().await {
        sequence = sequence.wrapping_add(1);
        let mut record = brood_ipc::encode_client_record(message, generation, sequence)?;
        loop {
            match producer.try_send(record) {
                Ok(()) => break,
                Err(returned) => {
                    record = returned;
                    tokio::task::yield_now().await;
                }
            }
        }
        notifications.notify()?;
    }
    Ok(())
}

async fn write_local_actor_messages(
    mut producer: brood_ipc::ActorMessageProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    mut receiver: mpsc::UnboundedReceiver<ActorMessage>,
) -> Result<()> {
    let mut sequence = 0_u64;
    while let Some(message) = receiver.recv().await {
        sequence = sequence.wrapping_add(1);
        let mut record = brood_ipc::ActorMessageRecord::new(
            generation,
            sequence,
            message.session,
            &message.sender,
            &message.recipient,
            &message.body,
        )?;
        loop {
            match producer.try_send(record) {
                Ok(()) => break,
                Err(returned) => {
                    record = returned;
                    tokio::task::yield_now().await;
                }
            }
        }
        notifications.notify()?;
    }
    Ok(())
}

fn spawn_catalog_forwarder(
    fd: &std::os::fd::OwnedFd,
    notify_fd: std::os::fd::OwnedFd,
    generation: u64,
    outbound: Outbound,
) -> Result<()> {
    let mut consumer = brood_ipc::CriticalConsumer::map(fd.as_fd(), generation)?;
    let notifications = brood_ipc::EventNotifier::from_owned_fd(notify_fd);
    thread::Builder::new()
        .name("brood-remote-catalog".to_string())
        .spawn(move || {
            let mut pending: Option<(u64, usize, usize, Vec<u8>)> = None;
            while notifications.wait().is_ok() {
                while let Some(record) = consumer.try_recv() {
                    match record.opcode {
                        brood_ipc::CATALOG_SNAPSHOT_BEGIN => {
                            pending = Some((
                                record.sequence,
                                usize::try_from(record.arguments[0]).unwrap_or(usize::MAX),
                                usize::try_from(record.arguments[1]).unwrap_or(usize::MAX),
                                Vec::new(),
                            ));
                        }
                        brood_ipc::CATALOG_SNAPSHOT_CHUNK => {
                            let Some((revision, _, _, payload)) = pending.as_mut() else {
                                continue;
                            };
                            if *revision != record.sequence {
                                pending = None;
                                continue;
                            }
                            let valid = usize::try_from(record.flags)
                                .unwrap_or(usize::MAX)
                                .min(brood_ipc::CATALOG_CHUNK_BYTES);
                            let mut bytes = [0_u8; brood_ipc::CATALOG_CHUNK_BYTES];
                            for (target, word) in bytes
                                .chunks_mut(size_of::<u64>())
                                .zip(record.arguments.iter())
                            {
                                target.copy_from_slice(&word.to_ne_bytes());
                            }
                            if let Some(chunk) = bytes.get(..valid) {
                                payload.extend_from_slice(chunk);
                            }
                        }
                        brood_ipc::CATALOG_SNAPSHOT_END => {
                            let Some((revision, expected_len, expected_chunks, payload)) =
                                pending.take()
                            else {
                                continue;
                            };
                            if revision != record.sequence
                                || payload.len() != expected_len
                                || expected_chunks
                                    != payload.len().div_ceil(brood_ipc::CATALOG_CHUNK_BYTES)
                            {
                                continue;
                            }
                            let Ok(catalog) = serde_json::from_slice::<LocalCatalog>(&payload)
                            else {
                                continue;
                            };
                            if outbound
                                .reliable
                                .send(ServerMessage::Catalog(CatalogSnapshot {
                                    revision,
                                    active_session: catalog.active_session,
                                    sessions: catalog.sessions,
                                    windows: catalog.windows,
                                }))
                                .is_err()
                            {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn remote catalog forwarder")?;
    Ok(())
}

fn spawn_frame_forwarder(
    target: ControlTarget,
    fd: &std::os::fd::OwnedFd,
    notify_fd: std::os::fd::OwnedFd,
    generation: u64,
    outbound: Outbound,
) -> Result<()> {
    let mut consumer = brood_ipc::CriticalConsumer::map(fd.as_fd(), generation)?;
    let notifications = brood_ipc::EventNotifier::from_owned_fd(notify_fd);
    thread::Builder::new()
        .name("brood-remote-frames".to_string())
        .spawn(move || {
            let mut pending: Option<PendingScene> = None;
            let mut resources = HashMap::new();
            while notifications.wait().is_ok() {
                while let Some(record) = consumer.try_recv() {
                    match record.opcode {
                        brood_ipc::FRAME_SCENE_BEGIN => {
                            pending = Some(PendingScene {
                                sequence: record.sequence,
                                expected_windows: usize::try_from(record.arguments[0])
                                    .unwrap_or(usize::MAX),
                                extent: [
                                    u32::try_from(record.arguments[1]).unwrap_or(u32::MAX),
                                    u32::try_from(record.arguments[2]).unwrap_or(u32::MAX),
                                ],
                                windows: Vec::new(),
                            });
                        }
                        brood_ipc::FRAME_WINDOW_SHM => {
                            let Some(scene) = pending.as_mut() else {
                                continue;
                            };
                            let resource = record.arguments[1];
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                resources.entry(resource)
                            {
                                let Ok(frame) =
                                    shared_memory_resource(&target, resource, generation)
                                else {
                                    return;
                                };
                                entry.insert(frame);
                            }
                            let (x, y) = unpack_i32_pair(record.arguments[2]);
                            let (width, height) = unpack_i32_pair(record.arguments[3]);
                            scene.windows.push(PendingWindow {
                                pane: record.arguments[0],
                                resource,
                                source: [x, y, width, height],
                                active: record.arguments[4] != 0,
                            });
                        }
                        brood_ipc::FRAME_SCENE_END => {
                            let Some(scene) = pending.take() else {
                                continue;
                            };
                            if scene.sequence != record.sequence
                                || scene.windows.len() != scene.expected_windows
                            {
                                return;
                            }
                            let mut windows = Vec::with_capacity(scene.windows.len());
                            for window in scene.windows {
                                let Some(frame) = resources.get(&window.resource) else {
                                    return;
                                };
                                let metadata = frame.image.metadata();
                                let Ok(pixels) = frame.image.pixels(scene.sequence) else {
                                    return;
                                };
                                let Ok(packet) = NetworkFramePacket::from_pixels(
                                    window.resource,
                                    scene.sequence,
                                    window.pane,
                                    metadata.width,
                                    metadata.height,
                                    metadata.pixel_format,
                                    metadata.color_space,
                                    pixels,
                                ) else {
                                    return;
                                };
                                if outbound.frames.send(packet).is_err() {
                                    return;
                                }
                                windows.push(WindowFrame {
                                    pane: window.pane,
                                    resource: FrameResourceRef::Network {
                                        resource: window.resource,
                                    },
                                    source: window.source,
                                    active: window.active,
                                });
                            }
                            if outbound
                                .latest
                                .send(ServerMessage::Scene(FrameScene {
                                    sequence: scene.sequence,
                                    extent: scene.extent,
                                    windows,
                                }))
                                .is_err()
                            {
                                return;
                            }
                        }
                        opcode @ (brood_ipc::FRAME_FOCUS_PRESENTATION
                        | brood_ipc::FRAME_CLOSE_PRESENTATION) => {
                            let message = if opcode == brood_ipc::FRAME_FOCUS_PRESENTATION {
                                ServerMessage::FocusPresentation
                            } else {
                                ServerMessage::ClosePresentation
                            };
                            if outbound.critical.send(message).is_err() {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn remote frame forwarder")?;
    Ok(())
}

fn spawn_actor_forwarder(
    fd: &std::os::fd::OwnedFd,
    notify_fd: std::os::fd::OwnedFd,
    generation: u64,
    outbound: Outbound,
) -> Result<()> {
    let mut consumer = brood_ipc::ActorMessageConsumer::map(fd.as_fd(), generation)?;
    let notifications = brood_ipc::EventNotifier::from_owned_fd(notify_fd);
    thread::Builder::new()
        .name("brood-remote-actors".to_string())
        .spawn(move || {
            while notifications.wait().is_ok() {
                while let Some(record) = consumer.try_recv() {
                    let message = (|| {
                        Some(ActorMessage {
                            session: record.session_id(),
                            sender: record.sender().ok()?.to_string(),
                            recipient: record.recipient().ok()?.to_string(),
                            body: record.body().ok()?.to_vec(),
                        })
                    })();
                    drop(record);
                    let Some(message) = message else {
                        continue;
                    };
                    if outbound
                        .reliable
                        .send(ServerMessage::Actor(message))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .context("spawn remote actor forwarder")?;
    Ok(())
}

async fn probe(
    connect: SocketAddr,
    server_name: &str,
    certificate: &Path,
    client_certificate: &Path,
    client_private_key: &Path,
) -> Result<()> {
    let endpoint = client_endpoint(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        fs::read(certificate)
            .with_context(|| format!("read certificate {}", certificate.display()))?,
        fs::read(client_certificate)
            .with_context(|| format!("read client certificate {}", client_certificate.display()))?,
        fs::read(client_private_key)
            .with_context(|| format!("read client private key {}", client_private_key.display()))?,
    )?;
    let mut network = connect_network(
        &endpoint,
        connect,
        server_name,
        EndpointCapabilities::remote(),
    )
    .await?;
    let (mut critical_send, _critical_receive) = network.take_lane(Lane::Critical)?.split();
    let (_reliable_send, mut reliable_receive) = network.take_lane(Lane::Reliable)?.split();
    let (_latest_send, mut latest_receive) = network.take_lane(Lane::LatestState)?.split();
    let (_frame_send, mut frame_receive) = network.take_lane(Lane::FrameResource)?.split();

    critical_send
        .send_client_message(&ClientMessage::Input(InputEvent::PresentationSize {
            width: 1280,
            height: 800,
            refresh_millihz: 60_000,
        }))
        .await?;

    let catalog = loop {
        if let ServerMessage::Catalog(catalog) = reliable_receive.receive_server_message().await? {
            break catalog;
        }
    };
    let (scene, packets) = loop {
        let ServerMessage::Scene(scene) = latest_receive.receive_server_message().await? else {
            continue;
        };
        if scene.windows.is_empty() {
            acknowledge_frame(&mut critical_send, scene.sequence).await?;
            continue;
        }
        let mut packets = Vec::with_capacity(scene.windows.len());
        let expected = scene
            .windows
            .iter()
            .map(|window| window.resource.id())
            .collect::<HashSet<_>>();
        while packets.len() < expected.len() {
            let packet = frame_receive.receive_frame().await?;
            if packet.sequence == scene.sequence && expected.contains(&packet.resource) {
                ensure!(
                    !packet.pixels()?.is_empty(),
                    "remote frame decompressed empty"
                );
                packets.push(packet);
            }
        }
        break (scene, packets);
    };
    acknowledge_frame(&mut critical_send, scene.sequence).await?;
    critical_send
        .send_client_message(&ClientMessage::Lifecycle(ClientLifecycle::Disconnect))
        .await?;
    println!(
        "OK remote transport generation={} catalog_revision={} sessions={} windows={} scene={} frame_packets={} compressed_bytes={}",
        network.connection_generation,
        catalog.revision,
        catalog.sessions.len(),
        catalog.windows.len(),
        scene.sequence,
        packets.len(),
        packets
            .iter()
            .map(|packet| packet.compressed_pixels.len())
            .sum::<usize>()
    );
    Ok(())
}

async fn acknowledge_frame(lane: &mut NetworkSendLane, sequence: u64) -> Result<()> {
    lane.send_client_message(&ClientMessage::Lifecycle(ClientLifecycle::FramePresented {
        sequence,
    }))
    .await
}

fn unpack_i32_pair(value: u64) -> (i32, i32) {
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
    let high = u32::try_from(value >> 32).unwrap_or_default();
    (
        i32::from_ne_bytes(low.to_ne_bytes()),
        i32::from_ne_bytes(high.to_ne_bytes()),
    )
}
