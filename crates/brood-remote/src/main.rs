use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    io::Write,
    mem::size_of,
    net::{Ipv4Addr, SocketAddr},
    os::{
        fd::AsFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use brood_cli::control::{
    ControlTarget, SharedMemoryFrame, complete_realm_artifact_request, handshake,
    install_session_init_artifact, open_presentation_fast_path, request_internal_command,
    resolve_control_target, shared_memory_resource, take_realm_artifact_requests,
};
use brood_protocol::{
    ActorMessage, ArtifactDigest, ArtifactMessage, ArtifactRealm, BackendActorRealm,
    BackendOperation, BackendRequest, BackendResponse, BackendResponseValue, CatalogSession,
    CatalogSnapshot, CatalogWindow, ClientLifecycle, ClientMessage, EndpointCapabilities,
    FrameResourceRef, FrameScene, FrameTransportKind, InputEvent, Lane, ServerMessage, WindowFrame,
};
use brood_transport::{
    ArtifactExchange, ArtifactExchangeEvent, ArtifactOffer, ArtifactTransfer, NetworkFramePacket,
    NetworkReceiveLane, NetworkSendLane, accept_network_connection, client_endpoint,
    connect_network, default_artifact_cache, digest_file, digest_hex, package_offers,
    server_endpoint,
};
use clap::{Parser, Subcommand};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

mod container;
mod ssh;

const ACTOR_LANE_CAPACITY: usize = 64;
const CRITICAL_EGRESS_CAPACITY: usize = 32;
const ACTOR_EGRESS_CAPACITY: usize = 128;
const RELIABLE_EGRESS_CAPACITY: usize = 32;
const LOCAL_INPUT_CAPACITY: usize = 512;
const LOCAL_ACTOR_CAPACITY: usize = 128;
const IPC_RETRY_YIELDS: u32 = 4;
const IPC_RETRY_INITIAL_DELAY: Duration = Duration::from_micros(50);
const IPC_RETRY_MAX_DELAY: Duration = Duration::from_millis(1);
const NETWORK_SETUP_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Build, copy, and start a backend over SSH, then launch the local client.
    Ssh(ssh::SshArgs),
    /// Start an SSH backend for an already-running presentation client.
    AttachSsh(ssh::SshArgs),
    /// Start a backend in a Podman container, then launch the local client.
    Container(container::ContainerArgs),
    /// Start a container backend for an already-running presentation client.
    AttachContainer(container::ContainerArgs),
    /// Run the in-container backend supervisor. This is an internal entry point.
    #[command(hide = true)]
    ContainerSession(container::ContainerSessionArgs),
    /// Keep a container alive while a backend is attached. This is an internal entry point.
    #[command(hide = true)]
    ContainerHold,
    /// Podman-compatible frontend which adds a Brood backend to new containers.
    Podman(container::PodmanArgs),
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
        /// Offer a WASM actor to the authenticated presentation client.
        #[arg(long = "offer-client-artifact", value_name = "ID=PATH")]
        offer_client_artifacts: Vec<String>,
        /// Authorize a client-offered session-realm artifact by stable ID.
        #[arg(long = "allow-session-artifact", value_name = "ID")]
        allow_session_artifacts: Vec<String>,
        /// Authorize one lazily hydrated session-init package by stable package ID.
        #[arg(long = "allow-session-package", value_name = "ID")]
        allow_session_packages: Vec<String>,
        /// Override the verified content-addressed artifact cache.
        #[arg(long = "artifact-cache", value_name = "PATH")]
        artifact_cache: Option<PathBuf>,
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
        /// Optional XKB keycode whose focused-pane pixel effect must be observed.
        #[arg(long)]
        keycode: Option<u32>,
        /// Offer a WASM actor to the remote session as ID from PATH.
        #[arg(long = "offer-session-artifact", value_name = "ID=PATH")]
        offer_session_artifacts: Vec<String>,
        /// Offer a lazily hydrated WASM package rooted at PATH under stable package ID.
        #[arg(long = "offer-session-package", value_name = "ID=ROOT_WASM_PATH")]
        offer_session_packages: Vec<String>,
        /// Authorize a relay-offered client-realm artifact by stable ID.
        #[arg(long = "allow-client-artifact", value_name = "ID")]
        allow_client_artifacts: Vec<String>,
        /// Override the verified content-addressed artifact cache.
        #[arg(long = "artifact-cache", value_name = "PATH")]
        artifact_cache: Option<PathBuf>,
    },
}

#[derive(Clone)]
struct Outbound {
    critical: mpsc::Sender<CriticalOutbound>,
    actors: mpsc::Sender<ServerMessage>,
    reliable: mpsc::Sender<ServerMessage>,
    scenes: tokio::sync::watch::Sender<Option<Arc<NetworkScene>>>,
}

struct NetworkScene {
    frames: Vec<NetworkFramePacket>,
    scene: FrameScene,
}

type SharedFrameResources = Arc<Mutex<HashMap<u64, Arc<SharedMemoryFrame>>>>;

#[derive(Clone, Default)]
struct TerminalRelayLatency(Arc<Mutex<TerminalRelayLatencyState>>);

#[derive(Default)]
struct TerminalRelayLatencyState {
    pending: BTreeMap<(u64, u64), TerminalInputTrace>,
    network_to_ipc_samples: VecDeque<u128>,
    backend_turnaround_samples: VecDeque<u128>,
    ipc_to_snapshot_samples: VecDeque<u128>,
    pending_reliable_send: VecDeque<Instant>,
    reliable_send_samples: VecDeque<u128>,
}

struct TerminalInputTrace {
    received: Instant,
    ipc_sent: Option<Instant>,
}

impl TerminalRelayLatency {
    fn record_input(&self, message: &ActorMessage) {
        let Some(key) = terminal_input_trace_key(message) else {
            return;
        };
        if let Ok(mut state) = self.0.lock() {
            state.pending.insert(
                key,
                TerminalInputTrace {
                    received: Instant::now(),
                    ipc_sent: None,
                },
            );
            while state.pending.len() > 4_096 {
                let Some(key) = state.pending.keys().next().copied() else {
                    break;
                };
                state.pending.remove(&key);
            }
        }
    }

    fn record_input_ipc_sent(&self, message: &ActorMessage) {
        let Some(key) = terminal_input_trace_key(message) else {
            return;
        };
        let now = Instant::now();
        if let Ok(mut state) = self.0.lock() {
            let sample = state.pending.get_mut(&key).map(|trace| {
                trace.ipc_sent = Some(now);
                now.duration_since(trace.received).as_micros()
            });
            if let Some(sample) = sample {
                push_latency_sample(&mut state.network_to_ipc_samples, sample);
            }
        }
    }

    fn record_snapshot(&self, message: &ActorMessage) {
        let Some((session, acknowledged)) = terminal_snapshot_trace_key(message) else {
            return;
        };
        let now = Instant::now();
        if let Ok(mut state) = self.0.lock() {
            let completed = state
                .pending
                .range((session, 0)..=(session, acknowledged))
                .map(|(key, _trace)| *key)
                .collect::<Vec<_>>();
            for key in completed {
                let Some(trace) = state.pending.remove(&key) else {
                    continue;
                };
                push_latency_sample(
                    &mut state.backend_turnaround_samples,
                    now.duration_since(trace.received).as_micros(),
                );
                if let Some(ipc_sent) = trace.ipc_sent {
                    push_latency_sample(
                        &mut state.ipc_to_snapshot_samples,
                        now.duration_since(ipc_sent).as_micros(),
                    );
                }
            }
            state.pending_reliable_send.push_back(now);
            if state.pending_reliable_send.len() > 4_096 {
                state.pending_reliable_send.pop_front();
            }
        }
    }

    fn record_snapshot_sent(&self) {
        let now = Instant::now();
        if let Ok(mut state) = self.0.lock()
            && let Some(queued) = state.pending_reliable_send.pop_front()
        {
            state
                .reliable_send_samples
                .push_back(now.duration_since(queued).as_micros());
            while state.reliable_send_samples.len() > 4_096 {
                state.reliable_send_samples.pop_front();
            }
        }
    }

    fn diagnostics(&self) -> serde_json::Value {
        let Ok(state) = self.0.lock() else {
            return serde_json::Value::Null;
        };
        let network_to_ipc = latency_summary(&state.network_to_ipc_samples);
        let backend_turnaround = latency_summary(&state.backend_turnaround_samples);
        let ipc_to_snapshot = latency_summary(&state.ipc_to_snapshot_samples);
        let reliable_send_queue = latency_summary(&state.reliable_send_samples);
        serde_json::json!({
            "network_to_ipc": network_to_ipc,
            "backend_turnaround": backend_turnaround,
            "ipc_to_snapshot": ipc_to_snapshot,
            "reliable_send_queue": reliable_send_queue,
            "pending": state.pending.len(),
            "pending_reliable_send": state.pending_reliable_send.len(),
        })
    }
}

fn push_latency_sample(samples: &mut VecDeque<u128>, sample: u128) {
    samples.push_back(sample);
    while samples.len() > 4_096 {
        samples.pop_front();
    }
}

fn terminal_input_trace_key(message: &ActorMessage) -> Option<(u64, u64)> {
    if !message.sender.starts_with("terminal-ui")
        || !message.recipient.starts_with("terminal-session")
        || message.body.first() != Some(&b'I')
    {
        return None;
    }
    let BackendActorRealm::Session(session) = message.realm else {
        return None;
    };
    let encoded = <[u8; 8]>::try_from(message.body.get(1..9)?).ok()?;
    Some((session, u64::from_le_bytes(encoded)))
}

fn terminal_snapshot_trace_key(message: &ActorMessage) -> Option<(u64, u64)> {
    if !message.sender.starts_with("terminal-session")
        || !message.recipient.starts_with("terminal-ui")
        || message.body.get(..4) != Some(b"BTRM")
        || message.body.get(4) != Some(&4)
    {
        return None;
    }
    let BackendActorRealm::Session(session) = message.realm else {
        return None;
    };
    let encoded = <[u8; 8]>::try_from(message.body.get(14..22)?).ok()?;
    Some((session, u64::from_le_bytes(encoded)))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "diagnostic latency statistics are approximate floating-point summaries"
)]
fn latency_summary(samples: &VecDeque<u128>) -> serde_json::Value {
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let mean = sorted.iter().map(|sample| *sample as f64).sum::<f64>() / sorted.len().max(1) as f64;
    let standard_deviation = (sorted
        .iter()
        .map(|sample| {
            let distance = *sample as f64 - mean;
            distance * distance
        })
        .sum::<f64>()
        / sorted.len().max(1) as f64)
        .sqrt();
    let percentile = |percentile: usize| {
        sorted
            .get(
                sorted
                    .len()
                    .saturating_mul(percentile)
                    .div_ceil(100)
                    .saturating_sub(1),
            )
            .copied()
            .unwrap_or_default()
    };
    serde_json::json!({
        "samples": sorted.len(),
        "mean_us": mean,
        "p50_us": percentile(50),
        "p95_us": percentile(95),
        "p99_us": percentile(99),
        "max_us": sorted.last().copied().unwrap_or_default(),
        "stddev_us": standard_deviation,
    })
}

enum CriticalOutbound {
    Message(ServerMessage),
    DisconnectAcknowledgement(oneshot::Sender<()>),
}

struct LocalCriticalInput {
    message: ClientMessage,
    published: Option<oneshot::Sender<()>>,
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
    resize_epoch: Option<u64>,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::GenerateCertificate {
            server_name,
            certificate,
            private_key,
        } => generate_certificate(&server_name, &certificate, &private_key),
        Command::Ssh(args) => ssh::deploy(&args),
        Command::AttachSsh(args) => ssh::attach(&args),
        Command::Container(args) => container::deploy(&args),
        Command::AttachContainer(args) => container::attach(&args),
        Command::ContainerSession(args) => container::run_session(&args),
        Command::ContainerHold => container::hold(),
        Command::Podman(args) => container::podman(&args),
        Command::Relay {
            listen,
            certificate,
            private_key,
            trusted_client_certificate,
            dbus_name,
            dbus_address,
            offer_client_artifacts,
            allow_session_artifacts,
            allow_session_packages,
            artifact_cache,
        } => {
            relay(
                listen,
                &certificate,
                &private_key,
                &trusted_client_certificate,
                resolve_control_target(dbus_name, dbus_address),
                &offer_client_artifacts,
                allow_session_artifacts,
                allow_session_packages,
                artifact_cache,
            )
            .await
        }
        Command::Probe {
            connect,
            server_name,
            certificate,
            client_certificate,
            client_private_key,
            keycode,
            offer_session_artifacts,
            offer_session_packages,
            allow_client_artifacts,
            artifact_cache,
        } => {
            probe(
                connect,
                &server_name,
                &certificate,
                &client_certificate,
                &client_private_key,
                keycode,
                &offer_session_artifacts,
                &offer_session_packages,
                allow_client_artifacts,
                artifact_cache,
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
    let mut key_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(key)
        .with_context(|| format!("open remote presentation private key {}", key.display()))?;
    key_file
        .set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure remote presentation private key {}", key.display()))?;
    key_file
        .write_all(&signing_key.serialize_der())
        .with_context(|| format!("write remote presentation private key {}", key.display()))?;
    Ok(())
}

#[cfg(test)]
mod certificate_tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use anyhow::{Result, ensure};

    use super::generate_certificate;

    #[test]
    fn generated_private_key_is_owner_only_even_when_replacing_a_file() -> Result<()> {
        let root = tempfile::tempdir()?;
        let certificate = root.path().join("server.der");
        let key = root.path().join("server-key.der");
        fs::write(&key, b"old key")?;
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644))?;

        generate_certificate("localhost", &certificate, &key)?;

        ensure!(fs::metadata(key)?.permissions().mode() & 0o777 == 0o600);
        Ok(())
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the relay CLI boundary combines network identity, local control, and artifact policy"
)]
async fn relay(
    listen: SocketAddr,
    certificate: &Path,
    private_key: &Path,
    trusted_client_certificate: &Path,
    target: ControlTarget,
    offer_specs: &[String],
    allowed_session_artifacts: Vec<String>,
    allowed_session_packages: Vec<String>,
    artifact_cache: Option<PathBuf>,
) -> Result<()> {
    ensure!(
        allowed_session_packages.len() <= 1,
        "a relay can hydrate at most one session-init package"
    );
    let artifact_offers = offer_specs
        .iter()
        .map(|spec| ArtifactOffer::parse(spec, ArtifactRealm::Client))
        .collect::<Result<Vec<_>>>()?;
    let artifact_cache = artifact_cache.unwrap_or_else(default_artifact_cache);
    let package_cache = artifact_cache.join("packages");
    let artifact_exchange = ArtifactExchange::new(
        ArtifactRealm::Session,
        allowed_session_artifacts,
        artifact_cache,
        artifact_offers,
    )?
    .with_allowed_packages(allowed_session_packages.clone());
    let session_package = allowed_session_packages.into_iter().next();
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
    let mut network = tokio::time::timeout(
        NETWORK_SETUP_TIMEOUT,
        accept_network_connection(
            incoming,
            EndpointCapabilities::remote(),
            fast_path.generation,
        ),
    )
    .await
    .context("remote presentation handshake timed out")??;
    let (critical_tx, critical_rx) = mpsc::channel(CRITICAL_EGRESS_CAPACITY);
    let (actor_tx, actor_rx) = mpsc::channel(ACTOR_EGRESS_CAPACITY);
    let (reliable_tx, reliable_rx) = mpsc::channel(RELIABLE_EGRESS_CAPACITY);
    let (scene_tx, scene_rx) = tokio::sync::watch::channel(None);
    let outbound = Outbound {
        critical: critical_tx.clone(),
        actors: actor_tx,
        reliable: reliable_tx,
        scenes: scene_tx,
    };
    let terminal_latency = TerminalRelayLatency::default();

    spawn_catalog_forwarder(
        &fast_path.catalog_fd,
        fast_path.catalog_notify_fd,
        fast_path.generation,
        network.backend,
        outbound.clone(),
    )?;
    let frame_resources = Arc::new(Mutex::new(HashMap::new()));
    spawn_frame_forwarder(
        target.clone(),
        &fast_path.frame_fd,
        fast_path.frame_notify_fd,
        fast_path.generation,
        network.backend,
        outbound.clone(),
        Arc::clone(&frame_resources),
    )?;
    spawn_actor_forwarder(
        &fast_path.presentation_message_fd,
        fast_path.presentation_message_notify_fd,
        fast_path.generation,
        outbound.clone(),
        terminal_latency.clone(),
    )?;

    let input = brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
    let input_notifications = brood_ipc::EventNotifier::from_owned_fd(fast_path.input_notify_fd);
    let actor_messages = brood_ipc::ActorMessageProducer::map(
        fast_path.actor_message_fd.as_fd(),
        fast_path.generation,
    )?;
    let actor_notifications =
        brood_ipc::EventNotifier::from_owned_fd(fast_path.actor_message_notify_fd);
    let backend_requests = brood_ipc::BackendMessageProducer::map(
        fast_path.backend_request_fd.as_fd(),
        fast_path.generation,
    )?;
    let backend_request_notifications =
        brood_ipc::EventNotifier::from_owned_fd(fast_path.backend_request_notify_fd);
    let backend_responses = brood_ipc::BackendMessageConsumer::map(
        fast_path.backend_response_fd.as_fd(),
        fast_path.generation,
    )?;
    let backend_response_notifications =
        brood_ipc::EventNotifier::from_owned_fd(fast_path.backend_response_notify_fd);
    let backend_commands = spawn_local_backend_bridge(
        backend_requests,
        backend_request_notifications,
        backend_responses,
        backend_response_notifications,
    )?;
    let (local_critical_input_tx, local_critical_input_rx) = mpsc::channel(LOCAL_INPUT_CAPACITY);
    let (local_latest_input_tx, local_latest_input_rx) = tokio::sync::watch::channel(None);
    let (local_actor_tx, local_actor_rx) = mpsc::channel(LOCAL_ACTOR_CAPACITY);
    let mut transport_tasks = tokio::task::JoinSet::new();
    transport_tasks.spawn(async move {
        write_local_input(
            input,
            input_notifications,
            fast_path.generation,
            local_critical_input_rx,
            local_latest_input_rx,
        )
        .await
        .context("local input writer stopped")
    });
    let actor_writer_latency = terminal_latency.clone();
    transport_tasks.spawn(async move {
        write_local_actor_messages(
            actor_messages,
            actor_notifications,
            fast_path.generation,
            local_actor_rx,
            actor_writer_latency,
        )
        .await
        .context("local actor writer stopped")
    });

    let (critical_send, mut critical_receive) = network.take_lane(Lane::Critical)?.split();
    let (session0_send, session0_receive) = network.take_lane(Lane::InteractiveSession0)?.split();
    let (session1_send, session1_receive) = network.take_lane(Lane::InteractiveSession1)?.split();
    let (session2_send, session2_receive) = network.take_lane(Lane::InteractiveSession2)?.split();
    let (session3_send, session3_receive) = network.take_lane(Lane::InteractiveSession3)?.split();
    let (daemon_send, daemon_receive) = network.take_lane(Lane::InteractiveDaemon)?.split();
    let (reliable_send, reliable_receive) = network.take_lane(Lane::Reliable)?.split();
    let (latest_send, latest_receive) = network.take_lane(Lane::LatestState)?.split();
    let (bulk_send, bulk_receive) = network.take_lane(Lane::Bulk)?.split();
    let (frame_send, _frame_receive) = network.take_lane(Lane::FrameResource)?.split();

    transport_tasks.spawn(async move {
        send_critical_messages(critical_send, critical_rx)
            .await
            .context("critical network sender stopped")
    });
    let actor_sender_latency = terminal_latency.clone();
    transport_tasks.spawn(async move {
        send_server_messages(
            [session0_send, session1_send, session2_send, session3_send],
            daemon_send,
            actor_rx,
            actor_sender_latency,
        )
        .await
        .context("actor network sender stopped")
    });
    transport_tasks.spawn(async move {
        send_reliable_messages(reliable_send, reliable_rx)
            .await
            .context("reliable network sender stopped")
    });
    transport_tasks.spawn(async move {
        send_network_scenes(frame_send, latest_send, scene_rx)
            .await
            .context("scene network sender stopped")
    });

    let (disconnect_tx, mut disconnect_rx) = oneshot::channel();
    let critical_input_tx = local_critical_input_tx;
    let disconnect_critical_tx = critical_tx;
    transport_tasks.spawn(async move {
        let result: Result<()> = async {
            loop {
                let message = critical_receive.receive_client_message().await?;
                let disconnect = message == ClientMessage::Lifecycle(ClientLifecycle::Disconnect);
                let (published, publication) = if disconnect {
                    let (published, publication) = oneshot::channel();
                    (Some(published), Some(publication))
                } else {
                    (None, None)
                };
                critical_input_tx
                    .send(LocalCriticalInput { message, published })
                    .await
                    .map_err(|_error| anyhow!("local input writer stopped"))?;
                if disconnect {
                    publication
                        .context("disconnect publication acknowledgement is missing")?
                        .await
                        .context("local input writer dropped disconnect publication")?;
                    critical_receive.wait_for_finish().await?;
                    let (acknowledged_tx, acknowledged_rx) = oneshot::channel();
                    disconnect_critical_tx
                        .send(CriticalOutbound::DisconnectAcknowledgement(acknowledged_tx))
                        .await
                        .map_err(|_error| anyhow!("critical network sender stopped"))?;
                    acknowledged_rx.await.context(
                        "critical network sender dropped the disconnect acknowledgement",
                    )?;
                    let _send_result = disconnect_tx.send(());
                    return Ok(());
                }
            }
        }
        .await;
        result.context("critical network receiver stopped")
    });
    transport_tasks.spawn(async move {
        receive_latest_input(latest_receive, local_latest_input_tx)
            .await
            .context("latest-state network receiver stopped")
    });
    for receive in [
        session0_receive,
        session1_receive,
        session2_receive,
        session3_receive,
    ] {
        let actor_input_tx = local_actor_tx.clone();
        let actor_input_latency = terminal_latency.clone();
        transport_tasks.spawn(async move {
            receive_actor_input(receive, actor_input_tx, actor_input_latency)
                .await
                .context("session actor network receiver stopped")
        });
    }
    let daemon_input_latency = terminal_latency.clone();
    transport_tasks.spawn(async move {
        receive_actor_input(daemon_receive, local_actor_tx, daemon_input_latency)
            .await
            .context("daemon actor network receiver stopped")
    });
    transport_tasks.spawn(async move {
        receive_reliable_input(
            reliable_receive,
            backend_commands,
            outbound,
            frame_resources,
        )
        .await
        .context("reliable network receiver stopped")
    });
    let mut artifact_task = tokio::spawn(exchange_session_artifacts(
        bulk_send,
        bulk_receive,
        artifact_exchange,
        target,
        session_package,
        package_cache,
    ));

    let result = tokio::select! {
        biased;
        disconnect = &mut disconnect_rx => {
            disconnect.context("remote client disconnected without a lifecycle message")
        }
        task = transport_tasks.join_next() => {
            match task {
                Some(Ok(Ok(()))) => Err(anyhow!("remote transport task stopped unexpectedly")),
                Some(Ok(Err(error))) => Err(error),
                Some(Err(error)) => Err(anyhow!(error).context("remote transport task panicked")),
                None => Err(anyhow!("remote transport task set is empty")),
            }
        }
        artifact = &mut artifact_task => {
            let artifact = match artifact {
                Ok(result) => result,
                Err(error) => Err(anyhow!(error).context("join remote artifact exchange")),
            };
            match tokio::time::timeout(
                std::time::Duration::from_secs(1),
                &mut disconnect_rx,
            ).await {
                Ok(disconnect) => {
                    if let Err(error) = &artifact {
                        eprintln!(
                            "remote artifact lane stopped while the client disconnected: \
                             {error:#}"
                        );
                    }
                    disconnect.context("remote client disconnected without a lifecycle message")
                }
                Err(_elapsed) => match artifact {
                    Ok(()) => Err(anyhow!("remote artifact exchange stopped unexpectedly")),
                    Err(error) => Err(error.context("remote artifact exchange failed")),
                },
            }
        }
    };
    transport_tasks.abort_all();
    artifact_task.abort();
    eprintln!(
        "remote terminal relay diagnostics {}",
        terminal_latency.diagnostics()
    );
    let diagnostics = network.diagnostics();
    eprintln!(
        "remote relay QUIC diagnostics {{\"rtt_us\":{},\"congestion_window\":{},\"congestion_events\":{},\"lost_packets\":{},\"lost_bytes\":{},\"sent_packets\":{},\"mtu\":{}}}",
        diagnostics.rtt_us,
        diagnostics.congestion_window,
        diagnostics.congestion_events,
        diagnostics.lost_packets,
        diagnostics.lost_bytes,
        diagnostics.sent_packets,
        diagnostics.mtu,
    );
    result
}

async fn exchange_session_artifacts(
    mut send: NetworkSendLane,
    mut receive: NetworkReceiveLane,
    mut exchange: ArtifactExchange,
    target: ControlTarget,
    session_package: Option<String>,
    package_cache: PathBuf,
) -> Result<()> {
    let mut outbound = VecDeque::from(exchange.initial_messages());
    let mut transfers = VecDeque::<ArtifactTransfer>::new();
    let mut waiting_by_digest = HashMap::<ArtifactDigest, Vec<u64>>::new();
    let mut active_package_roots = HashMap::<String, PathBuf>::new();
    let mut unresolved = VecDeque::<(u64, String, std::time::Instant)>::new();
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(100));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // `read_exact` is not cancellation safe: dropping it after it has consumed
    // only part of a frame loses those bytes. Keep one receive future alive
    // across package-request poll ticks, which matters on higher-latency links.
    let mut incoming = Box::pin(receive_client_artifact(&mut receive));
    loop {
        while let Some(message) = outbound.pop_front() {
            send.send_server_message(&ServerMessage::Artifact(message))
                .await?;
        }
        while let Some(transfer) = transfers.pop_front() {
            for message in transfer.messages()? {
                send.send_server_message(&ServerMessage::Artifact(message?))
                    .await?;
            }
        }

        tokio::select! {
            message = &mut incoming => {
                let message = message?;
                drop(incoming);
                let update = exchange.handle(message)?;
                apply_session_artifact_update(
                    update,
                    &mut exchange,
                    &mut outbound,
                    &mut transfers,
                    &target,
                    &package_cache,
                    &mut waiting_by_digest,
                    &mut active_package_roots,
                ).await?;
                incoming = Box::pin(receive_client_artifact(&mut receive));
            }
            _ = poll.tick() => {
                let poll_target = target.clone();
                let requests = tokio::task::spawn_blocking(move || {
                    take_realm_artifact_requests(&poll_target)
                })
                .await
                .context("join realm artifact request poll")??;
                let now = std::time::Instant::now();
                unresolved.extend(
                    requests
                        .into_iter()
                        .map(|(request, path)| (request, path, now)),
                );

                let pending = unresolved.len();
                for _ in 0..pending {
                    let Some((request, path, first_seen)) = unresolved.pop_front() else {
                        break;
                    };
                    let Some(package) = session_package.as_deref() else {
                        complete_missing_member(
                            &target,
                            request,
                            false,
                            "relay has no authorized session package".to_string(),
                        ).await?;
                        continue;
                    };
                    let Some(digest) = exchange.package_member_digest(package, &path) else {
                        if first_seen.elapsed() < std::time::Duration::from_secs(10) {
                            unresolved.push_back((request, path, first_seen));
                        } else {
                            complete_missing_member(
                                &target,
                                request,
                                false,
                                format!("client did not advertise package member {package:?}:{path:?}"),
                            ).await?;
                        }
                        continue;
                    };
                    eprintln!(
                        "requesting missing session package member package={} path={} digest={}",
                        package,
                        path,
                        digest_hex(digest)
                    );
                    waiting_by_digest.entry(digest).or_default().push(request);
                    let update = exchange.request_package_member(package, &path)?;
                    apply_session_artifact_update(
                        update,
                        &mut exchange,
                        &mut outbound,
                        &mut transfers,
                        &target,
                        &package_cache,
                        &mut waiting_by_digest,
                        &mut active_package_roots,
                    ).await?;
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "artifact installation updates several independent connection-scoped registries"
)]
async fn apply_session_artifact_update(
    update: brood_transport::ArtifactExchangeUpdate,
    exchange: &mut ArtifactExchange,
    outbound: &mut VecDeque<ArtifactMessage>,
    transfers: &mut VecDeque<ArtifactTransfer>,
    target: &ControlTarget,
    package_cache: &Path,
    waiting_by_digest: &mut HashMap<ArtifactDigest, Vec<u64>>,
    active_package_roots: &mut HashMap<String, PathBuf>,
) -> Result<()> {
    outbound.extend(update.outbound);
    transfers.extend(update.transfers);
    for event in update.events {
        match event {
            ArtifactExchangeEvent::Installed(artifact) => {
                let digest = artifact.manifest.digest;
                let waiting = waiting_by_digest.remove(&digest).unwrap_or_default();
                let package_digest = artifact
                    .manifest
                    .package
                    .as_ref()
                    .filter(|member| member.root)
                    .and_then(|member| exchange.package_digest(&member.package));
                let install_result = install_session_artifact(
                    target,
                    &artifact,
                    package_digest,
                    package_cache,
                    &waiting,
                    active_package_roots,
                )
                .await;
                if let Err(error) = &install_result
                    && artifact
                        .manifest
                        .package
                        .as_ref()
                        .is_some_and(|member| !member.root)
                {
                    for request in waiting {
                        if let Err(completion_error) =
                            complete_missing_member(target, request, false, format!("{error:#}"))
                                .await
                        {
                            eprintln!(
                                "failed to reject realm artifact request {request}: {completion_error:#}"
                            );
                        }
                    }
                }
                let completion = match &install_result {
                    Ok(()) => Ok(()),
                    Err(error) => Err(format!("{error:#}")),
                };
                outbound.push_back(exchange.finish_installation(digest, completion)?);
                match install_result {
                    Ok(()) => eprintln!(
                        "installed remote session artifact id={} digest={}",
                        artifact.manifest.id,
                        digest_hex(digest)
                    ),
                    Err(error) => eprintln!(
                        "rejected remote session artifact id={} digest={} reason={error:#}",
                        artifact.manifest.id,
                        digest_hex(digest)
                    ),
                }
            }
            ArtifactExchangeEvent::PeerAccepted(manifest) => {
                eprintln!(
                    "remote client accepted artifact id={} digest={}",
                    manifest.id,
                    digest_hex(manifest.digest)
                );
            }
            ArtifactExchangeEvent::PeerRejected {
                manifest,
                digest,
                reason,
            } => {
                eprintln!(
                    "remote client rejected artifact id={} digest={} reason={reason}",
                    manifest
                        .as_ref()
                        .map_or("<unknown>", |value| value.id.as_str()),
                    digest_hex(digest)
                );
            }
        }
    }
    Ok(())
}

async fn install_session_artifact(
    target: &ControlTarget,
    artifact: &brood_transport::InstalledArtifact,
    package_digest: Option<ArtifactDigest>,
    package_cache: &Path,
    waiting: &[u64],
    active_package_roots: &mut HashMap<String, PathBuf>,
) -> Result<()> {
    ensure!(
        artifact.manifest.realm == ArtifactRealm::Session,
        "remote artifact {:?} targets the wrong realm",
        artifact.manifest.id
    );
    if let Some(member) = &artifact.manifest.package {
        if member.root {
            ensure!(
                waiting.is_empty(),
                "session package root unexpectedly satisfied a child request"
            );
            let package_root = package_cache.join(&member.package).join(digest_hex(
                package_digest.context("session package catalogue is incomplete")?,
            ));
            let path = materialize_package_member(&package_root, artifact)?;
            let install_target = target.clone();
            let installed = tokio::task::spawn_blocking(move || {
                install_session_init_artifact(&install_target, &path)
            })
            .await
            .context("join session init artifact installation")??;
            ensure!(
                installed,
                "backend did not install the session init artifact"
            );
            active_package_roots.insert(member.package.clone(), package_root);
        } else {
            ensure!(
                !waiting.is_empty(),
                "package member was received without a backend request"
            );
            let package_root = active_package_roots.get(&member.package).with_context(|| {
                format!(
                    "package member {:?} arrived before its init root",
                    member.package
                )
            })?;
            materialize_package_member(package_root, artifact)?;
            for request in waiting {
                complete_missing_member(target, *request, true, String::new()).await?;
            }
        }
        return Ok(());
    }

    let install_target = target.clone();
    let id = artifact.manifest.id.clone();
    let path = artifact.path.clone();
    let response = tokio::task::spawn_blocking(move || {
        request_internal_command(
            &install_target,
            &[
                "plug".to_string(),
                "--id".to_string(),
                id,
                path.display().to_string(),
            ],
            false,
        )
    })
    .await
    .context("join session artifact installation")??;
    ensure!(
        response.starts_with("OK "),
        "session rejected remote artifact installation: {}",
        response.trim()
    );
    Ok(())
}

fn materialize_package_member(
    package_root: &Path,
    artifact: &brood_transport::InstalledArtifact,
) -> Result<PathBuf> {
    let member = artifact
        .manifest
        .package
        .as_ref()
        .context("artifact is not a package member")?;
    let destination = package_root.join(&member.path);
    ensure!(
        destination.starts_with(package_root),
        "package member escaped its materialization root"
    );
    if destination.is_file()
        && fs::metadata(&destination)?.len() == artifact.manifest.bytes
        && digest_file(&destination)? == artifact.manifest.digest
    {
        return Ok(destination);
    }
    let parent = destination
        .parent()
        .context("package member has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create package materialization root {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".artifact-{}-{}.part",
        std::process::id(),
        digest_hex(artifact.manifest.digest)
    ));
    let result = (|| {
        fs::copy(&artifact.path, &temporary).with_context(|| {
            format!(
                "copy package member {} to {}",
                artifact.path.display(),
                temporary.display()
            )
        })?;
        ensure!(
            fs::metadata(&temporary)?.len() == artifact.manifest.bytes
                && digest_file(&temporary)? == artifact.manifest.digest,
            "materialized package member failed content verification"
        );
        fs::rename(&temporary, &destination).with_context(|| {
            format!(
                "publish package member {} as {}",
                temporary.display(),
                destination.display()
            )
        })
    })();
    if result.is_err() {
        drop(fs::remove_file(&temporary));
    }
    result?;
    Ok(destination)
}

async fn complete_missing_member(
    target: &ControlTarget,
    request: u64,
    success: bool,
    message: String,
) -> Result<()> {
    let completion_target = target.clone();
    let completed = tokio::task::spawn_blocking(move || {
        complete_realm_artifact_request(&completion_target, request, success, message)
    })
    .await
    .context("join realm artifact request completion")??;
    ensure!(
        completed,
        "backend no longer has realm artifact request {request}"
    );
    Ok(())
}

async fn receive_client_artifact(lane: &mut NetworkReceiveLane) -> Result<ArtifactMessage> {
    let message = lane.receive_client_message().await?;
    let ClientMessage::Artifact(message) = message else {
        bail!("non-artifact client message reached the artifact lane");
    };
    Ok(message)
}

async fn receive_latest_input(
    mut lane: brood_transport::NetworkReceiveLane,
    sender: tokio::sync::watch::Sender<Option<ClientMessage>>,
) -> Result<()> {
    loop {
        let message = lane.receive_client_message().await?;
        sender
            .send(Some(message))
            .map_err(|_error| anyhow!("local input writer stopped"))?;
    }
}

async fn receive_actor_input(
    mut lane: brood_transport::NetworkReceiveLane,
    sender: mpsc::Sender<ActorMessage>,
    terminal_latency: TerminalRelayLatency,
) -> Result<()> {
    loop {
        let ClientMessage::Actor(message) = lane.receive_client_message().await? else {
            bail!("non-actor client message reached the interactive lane");
        };
        if message.sender.starts_with("terminal-ui")
            && message.recipient.starts_with("terminal-session")
            && message.body.first() == Some(&b'I')
        {
            terminal_latency.record_input(&message);
        }
        sender
            .send(message)
            .await
            .map_err(|_error| anyhow!("local actor writer stopped"))?;
    }
}

struct LocalBackendCommand {
    request: BackendRequest,
    response: oneshot::Sender<Result<BackendResponse, String>>,
}

fn spawn_local_backend_bridge(
    mut requests: brood_ipc::BackendMessageProducer,
    request_notifications: brood_ipc::EventNotifier,
    mut responses: brood_ipc::BackendMessageConsumer,
    response_notifications: brood_ipc::EventNotifier,
) -> Result<mpsc::Sender<LocalBackendCommand>> {
    let (sender, mut receiver) = mpsc::channel::<LocalBackendCommand>(RELIABLE_EGRESS_CAPACITY);
    std::thread::Builder::new()
        .name("brood-relay-backend-ipc".to_string())
        .spawn(move || {
            while let Some(command) = receiver.blocking_recv() {
                let id = command.request.id;
                let value = brood_ipc::BackendMessageValue::Request(command.request);
                let result = (|| -> Result<BackendResponse, String> {
                    requests
                        .try_send(&value)
                        .map_err(|error| format!("send backend IPC request: {error}"))?;
                    request_notifications
                        .notify()
                        .map_err(|error| format!("notify backend IPC request: {error}"))?;
                    loop {
                        response_notifications
                            .wait()
                            .map_err(|error| format!("wait for backend IPC response: {error}"))?;
                        let Some(message) = responses.try_recv() else {
                            continue;
                        };
                        let message = message
                            .map_err(|error| format!("decode backend IPC response: {error}"))?;
                        match message.value {
                            brood_ipc::BackendMessageValue::Response(response)
                                if response.id == id =>
                            {
                                return Ok(response);
                            }
                            brood_ipc::BackendMessageValue::Response(response) => {
                                return Err(format!(
                                    "backend IPC response ID mismatch: expected {id}, received {}",
                                    response.id
                                ));
                            }
                            brood_ipc::BackendMessageValue::Request(_) => {
                                return Err("backend request reached the response lane".to_string());
                            }
                        }
                    }
                })();
                if command.response.send(result).is_err() {
                    return;
                }
            }
        })
        .context("spawn relay backend IPC bridge")?;
    Ok(sender)
}

async fn receive_reliable_input(
    mut lane: brood_transport::NetworkReceiveLane,
    backend_commands: mpsc::Sender<LocalBackendCommand>,
    outbound: Outbound,
    frame_resources: SharedFrameResources,
) -> Result<()> {
    loop {
        match lane.receive_client_message().await? {
            ClientMessage::BackendRequest(request) => {
                let (response_tx, response_rx) = oneshot::channel();
                backend_commands
                    .send(LocalBackendCommand {
                        request,
                        response: response_tx,
                    })
                    .await
                    .map_err(|_error| anyhow!("local backend IPC bridge stopped"))?;
                let response = response_rx
                    .await
                    .context("local backend IPC bridge dropped its response")?
                    .map_err(anyhow::Error::msg)?;
                outbound
                    .reliable
                    .send(ServerMessage::BackendResponse(response))
                    .await
                    .map_err(|_error| anyhow!("remote reliable sender stopped"))?;
            }
            ClientMessage::ReleaseResource { resource } => {
                frame_resources
                    .lock()
                    .map_err(|_poisoned| anyhow!("shared frame resource registry is poisoned"))?
                    .remove(&resource);
            }
            ClientMessage::Actor(_)
            | ClientMessage::Input(_)
            | ClientMessage::Lifecycle(_)
            | ClientMessage::Artifact(_) => {
                bail!("critical client message reached the reliable lane");
            }
        }
    }
}

async fn send_server_messages(
    session_lanes: [NetworkSendLane; 4],
    daemon_lane: NetworkSendLane,
    mut receiver: mpsc::Receiver<ServerMessage>,
    terminal_latency: TerminalRelayLatency,
) -> Result<()> {
    let mut senders = Vec::with_capacity(5);
    let mut tasks = tokio::task::JoinSet::new();
    for lane in session_lanes {
        let (sender, lane_receiver) = mpsc::channel(ACTOR_LANE_CAPACITY);
        senders.push(sender);
        tasks.spawn(send_server_actor_lane(
            lane,
            lane_receiver,
            terminal_latency.clone(),
        ));
    }
    let (daemon_sender, daemon_receiver) = mpsc::channel(ACTOR_LANE_CAPACITY);
    senders.push(daemon_sender);
    tasks.spawn(send_server_actor_lane(
        daemon_lane,
        daemon_receiver,
        terminal_latency,
    ));

    loop {
        tokio::select! {
            completed = tasks.join_next(), if !tasks.is_empty() => {
                return completed
                    .context("remote actor sender set is empty")?
                    .context("remote actor sender panicked")?;
            }
            message = receiver.recv() => {
                let Some(message) = message else {
                    break;
                };
                let lane = match &message {
                    ServerMessage::Actor(message) => match message.realm {
                        brood_protocol::BackendActorRealm::Daemon => 4,
                        brood_protocol::BackendActorRealm::Session(session) => {
                            usize::try_from(session % 4).unwrap_or_default()
                        }
                    },
                    _ => 0,
                };
                // Actor records are lossless. Await capacity in the selected
                // per-realm FIFO instead of treating transient saturation as
                // a transport failure.
                send_retained(
                    senders.get(lane).context("remote actor lane index is invalid")?,
                    message,
                    "remote actor sender stopped",
                ).await?;
            }
        }
    }
    drop(senders);
    while let Some(completed) = tasks.join_next().await {
        completed.context("remote actor sender panicked")??;
    }
    Ok(())
}

async fn send_retained<T>(sender: &mpsc::Sender<T>, value: T, closed: &str) -> Result<()> {
    sender
        .send(value)
        .await
        .map_err(|_error| anyhow!(closed.to_string()))
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;

    #[tokio::test]
    async fn server_actor_fanout_waits_for_capacity_without_losing_the_record() -> Result<()> {
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .send(1_u8)
            .await
            .map_err(|_error| anyhow!("test receiver stopped"))?;
        let pending = send_retained(&sender, 2_u8, "closed");
        tokio::pin!(pending);
        ensure!(
            tokio::time::timeout(Duration::from_millis(1), pending.as_mut())
                .await
                .is_err()
        );
        ensure!(receiver.recv().await == Some(1));
        pending.await?;
        ensure!(receiver.recv().await == Some(2));
        Ok(())
    }
}

async fn send_server_actor_lane(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::Receiver<ServerMessage>,
    terminal_latency: TerminalRelayLatency,
) -> Result<()> {
    while let Some(message) = receiver.recv().await {
        let terminal_snapshot = matches!(
            &message,
            ServerMessage::Actor(message)
                if message.sender.starts_with("terminal-session")
                    && message.recipient.starts_with("terminal-ui")
                    && message.body.starts_with(b"BTRM")
        );
        lane.send_server_message(&message).await?;
        if terminal_snapshot {
            terminal_latency.record_snapshot_sent();
        }
    }
    Ok(())
}

async fn send_reliable_messages(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::Receiver<ServerMessage>,
) -> Result<()> {
    while let Some(message) = receiver.recv().await {
        lane.send_server_message(&message).await?;
    }
    Ok(())
}

async fn send_critical_messages(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::Receiver<CriticalOutbound>,
) -> Result<()> {
    while let Some(command) = receiver.recv().await {
        match command {
            CriticalOutbound::Message(message) => {
                lane.send_server_message(&message).await?;
            }
            CriticalOutbound::DisconnectAcknowledgement(completion) => {
                lane.send_server_message(&ServerMessage::DisconnectAcknowledged)
                    .await?;
                lane.finish().await?;
                let _send_result = completion.send(());
                return std::future::pending::<Result<()>>().await;
            }
        }
    }
    Ok(())
}

async fn send_network_scenes(
    mut frames: NetworkSendLane,
    mut latest: NetworkSendLane,
    mut receiver: tokio::sync::watch::Receiver<Option<Arc<NetworkScene>>>,
) -> Result<()> {
    loop {
        receiver
            .changed()
            .await
            .context("network scene forwarder stopped")?;
        let Some(update) = receiver.borrow_and_update().clone() else {
            continue;
        };
        latest
            .send_server_message(&ServerMessage::Scene(update.scene.clone()))
            .await?;
        for frame in &update.frames {
            frames.send_frame(frame).await?;
        }
    }
}

async fn write_local_input(
    mut producer: brood_ipc::CriticalProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    mut critical: mpsc::Receiver<LocalCriticalInput>,
    mut latest: tokio::sync::watch::Receiver<Option<ClientMessage>>,
) -> Result<()> {
    let mut sequence = 0_u64;
    let mut critical_open = true;
    loop {
        let (message, published) = tokio::select! {
            biased;
            input = critical.recv(), if critical_open => {
                let Some(input) = input else {
                    critical_open = false;
                    continue;
                };
                (input.message, input.published)
            }
            changed = latest.changed() => {
                changed.context("latest-state input forwarder stopped")?;
                let Some(message) = latest.borrow_and_update().clone() else {
                    continue;
                };
                (message, None)
            }
        };
        sequence = sequence.wrapping_add(1);
        let mut record = brood_ipc::encode_client_record(message, generation, sequence)?;
        let mut retry = IpcRetry::default();
        loop {
            match producer.try_send(record) {
                Ok(()) => break,
                Err(returned) => {
                    record = returned;
                    retry.wait().await;
                }
            }
        }
        notifications.notify()?;
        if let Some(published) = published {
            let _send_result = published.send(());
        }
    }
}

async fn write_local_actor_messages(
    mut producer: brood_ipc::ActorMessageProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    mut receiver: mpsc::Receiver<ActorMessage>,
    terminal_latency: TerminalRelayLatency,
) -> Result<()> {
    let mut sequence = 0_u64;
    while let Some(message) = receiver.recv().await {
        sequence = sequence.wrapping_add(1);
        let mut record = brood_ipc::ActorMessageRecord::new(
            generation,
            sequence,
            message.realm,
            &message.sender,
            &message.recipient,
            &message.body,
        )?;
        let mut retry = IpcRetry::default();
        loop {
            match producer.try_send(record) {
                Ok(()) => break,
                Err(returned) => {
                    record = returned;
                    retry.wait().await;
                }
            }
        }
        notifications.notify()?;
        terminal_latency.record_input_ipc_sent(&message);
    }
    Ok(())
}

#[derive(Default)]
struct IpcRetry {
    waits: u32,
}

impl IpcRetry {
    async fn wait(&mut self) {
        if self.waits < IPC_RETRY_YIELDS {
            self.waits += 1;
            tokio::task::yield_now().await;
            return;
        }
        let exponent = self.waits.saturating_sub(IPC_RETRY_YIELDS).min(5);
        let multiplier = 1_u32 << exponent;
        let delay = IPC_RETRY_INITIAL_DELAY
            .saturating_mul(multiplier)
            .min(IPC_RETRY_MAX_DELAY);
        self.waits = self.waits.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

fn spawn_catalog_forwarder(
    fd: &std::os::fd::OwnedFd,
    notify_fd: std::os::fd::OwnedFd,
    generation: u64,
    backend: brood_protocol::BackendId,
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
                                .blocking_send(ServerMessage::Catalog(CatalogSnapshot {
                                    backend,
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
    backend: brood_protocol::BackendId,
    outbound: Outbound,
    resources: SharedFrameResources,
) -> Result<()> {
    let mut consumer = brood_ipc::CriticalConsumer::map(fd.as_fd(), generation)?;
    let notifications = brood_ipc::EventNotifier::from_owned_fd(notify_fd);
    thread::Builder::new()
        .name("brood-remote-frames".to_string())
        .spawn(move || {
            let mut pending: Option<PendingScene> = None;
            while notifications.wait().is_ok() {
                let mut newest_complete = None;
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
                            let Ok(window) = brood_ipc::decode_window_frame_record(&record) else {
                                return;
                            };
                            let resource = window.resource;
                            let Ok(mut resources) = resources.lock() else {
                                return;
                            };
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                resources.entry(resource)
                            {
                                let Ok(frame) =
                                    shared_memory_resource(&target, resource, generation)
                                else {
                                    return;
                                };
                                entry.insert(Arc::new(frame));
                            }
                            scene.windows.push(PendingWindow {
                                pane: window.pane,
                                resource,
                                source: window.source,
                                active: window.active,
                                resize_epoch: window.resize_epoch,
                            });
                        }
                        brood_ipc::FRAME_MANAGED_RESIZE_STATUS => {
                            let Ok(status) =
                                brood_ipc::decode_managed_resize_status_record(&record)
                            else {
                                return;
                            };
                            if outbound
                                .critical
                                .blocking_send(CriticalOutbound::Message(
                                    ServerMessage::ManagedResizeStatus(status),
                                ))
                                .is_err()
                            {
                                return;
                            }
                        }
                        brood_ipc::FRAME_SCENE_END => {
                            let Some(scene) = pending.take() else {
                                continue;
                            };
                            if scene.sequence != record.sequence
                                || scene.windows.len() != scene.expected_windows
                            {
                                continue;
                            }
                            newest_complete = Some(scene);
                        }
                        opcode @ (brood_ipc::FRAME_FOCUS_PRESENTATION
                        | brood_ipc::FRAME_CLOSE_PRESENTATION) => {
                            let message = if opcode == brood_ipc::FRAME_FOCUS_PRESENTATION {
                                ServerMessage::FocusPresentation
                            } else {
                                ServerMessage::ClosePresentation
                            };
                            if outbound
                                .critical
                                .blocking_send(CriticalOutbound::Message(message))
                                .is_err()
                            {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(scene) = newest_complete else {
                    continue;
                };
                let Ok(scene) = encode_network_scene(backend, scene, &resources) else {
                    continue;
                };
                if outbound.scenes.send(Some(Arc::new(scene))).is_err() {
                    return;
                }
            }
        })
        .context("spawn remote frame forwarder")?;
    Ok(())
}

fn encode_network_scene(
    backend: brood_protocol::BackendId,
    scene: PendingScene,
    resources: &SharedFrameResources,
) -> Result<NetworkScene> {
    let mut windows = Vec::with_capacity(scene.windows.len());
    let mut frames = Vec::with_capacity(scene.windows.len());
    let scene_frames = {
        let resources = resources
            .lock()
            .map_err(|_poisoned| anyhow!("shared frame resource registry is poisoned"))?;
        scene
            .windows
            .iter()
            .map(|window| {
                resources
                    .get(&window.resource)
                    .cloned()
                    .with_context(|| format!("frame resource {} is unavailable", window.resource))
            })
            .collect::<Result<Vec<_>>>()?
    };
    for (window, frame) in scene.windows.into_iter().zip(scene_frames) {
        let metadata = frame.image.metadata();
        let pixels = frame.image.pixels(scene.sequence)?;
        frames.push(NetworkFramePacket::from_pixels(
            window.resource,
            scene.sequence,
            window.pane,
            metadata.width,
            metadata.height,
            metadata.pixel_format,
            metadata.color_space,
            pixels,
        )?);
        windows.push(WindowFrame {
            pane: window.pane,
            resource: FrameResourceRef::Network {
                resource: window.resource,
            },
            source: window.source,
            active: window.active,
            resize_epoch: window.resize_epoch,
        });
    }
    Ok(NetworkScene {
        frames,
        scene: FrameScene {
            backend,
            sequence: scene.sequence,
            extent: scene.extent,
            windows,
        },
    })
}

fn spawn_actor_forwarder(
    fd: &std::os::fd::OwnedFd,
    notify_fd: std::os::fd::OwnedFd,
    generation: u64,
    outbound: Outbound,
    terminal_latency: TerminalRelayLatency,
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
                            realm: record.realm().ok()?,
                            sender: record.sender().ok()?.to_string(),
                            recipient: record.recipient().ok()?.to_string(),
                            body: record.body().ok()?.to_vec(),
                        })
                    })();
                    drop(record);
                    let Some(message) = message else {
                        continue;
                    };
                    if message.sender.starts_with("terminal-session")
                        && message.recipient.starts_with("terminal-ui")
                        && message.body.starts_with(b"BTRM")
                    {
                        terminal_latency.record_snapshot(&message);
                    }
                    if outbound
                        .actors
                        .blocking_send(ServerMessage::Actor(message))
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

#[allow(
    clippy::too_many_arguments,
    reason = "the probe CLI boundary combines network identity, behavior checks, and artifact policy"
)]
async fn probe(
    connect: SocketAddr,
    server_name: &str,
    certificate: &Path,
    client_certificate: &Path,
    client_private_key: &Path,
    keycode: Option<u32>,
    offer_specs: &[String],
    package_specs: &[String],
    allowed_client_artifacts: Vec<String>,
    artifact_cache: Option<PathBuf>,
) -> Result<()> {
    let mut artifact_offers = offer_specs
        .iter()
        .map(|spec| ArtifactOffer::parse(spec, ArtifactRealm::Session))
        .collect::<Result<Vec<_>>>()?;
    for spec in package_specs {
        artifact_offers.extend(package_offers(spec, ArtifactRealm::Session)?);
    }
    let offered_artifacts = artifact_offers.len();
    let expected_client_artifacts = allowed_client_artifacts.len();
    let artifact_exchange = ArtifactExchange::new(
        ArtifactRealm::Client,
        allowed_client_artifacts,
        artifact_cache.unwrap_or_else(default_artifact_cache),
        artifact_offers,
    )?;
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
        brood_protocol::BackendId(1),
        EndpointCapabilities::remote(),
    )
    .await?;
    let (mut critical_send, mut critical_receive) = network.take_lane(Lane::Critical)?.split();
    let (mut reliable_send, mut reliable_receive) = network.take_lane(Lane::Reliable)?.split();
    let (_latest_send, mut latest_receive) = network.take_lane(Lane::LatestState)?.split();
    let (mut bulk_send, mut bulk_receive) = network.take_lane(Lane::Bulk)?.split();
    let (_frame_send, mut frame_receive) = network.take_lane(Lane::FrameResource)?.split();

    let (artifacts_sent, artifacts_received) = probe_artifacts(
        &mut bulk_send,
        &mut bulk_receive,
        artifact_exchange,
        offered_artifacts,
        expected_client_artifacts,
    )
    .await?;

    critical_send
        .send_client_message(&ClientMessage::Input(InputEvent::PresentationSize {
            width: 1280,
            height: 800,
            refresh_millihz: 60_000,
            scale_120: 120,
        }))
        .await?;

    let catalog = loop {
        if let ServerMessage::Catalog(catalog) = reliable_receive
            .receive_server_message()
            .await
            .context("receive initial remote catalog")?
        {
            break catalog;
        }
    };
    reliable_send
        .send_client_message(&ClientMessage::BackendRequest(BackendRequest {
            id: 1,
            operation: BackendOperation::Control {
                argv: vec!["status".to_string()],
            },
        }))
        .await?;
    let backend_response = loop {
        if let ServerMessage::BackendResponse(response) = reliable_receive
            .receive_server_message()
            .await
            .context("receive remote backend response")?
            && response.id == 1
        {
            break response;
        }
    };
    let backend_status = match backend_response.result {
        Ok(BackendResponseValue::Control(response)) => response,
        Ok(BackendResponseValue::Session(_)) => {
            bail!("remote status request returned a session response")
        }
        Err(error) => bail!("remote status request failed: {error}"),
    };
    ensure!(
        !backend_status.trim().is_empty(),
        "remote status response was empty"
    );
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
    let (input_scene, input_frame_packets) = if let Some(keycode) = keycode {
        let pane = scene
            .windows
            .first()
            .context("non-empty scene lost its first pane")?
            .pane;
        let original_pixels = packets
            .iter()
            .find(|packet| packet.pane == pane)
            .context("initial scene did not include its focused pane")?
            .pixels()?;
        critical_send
            .send_client_message(&ClientMessage::Input(InputEvent::WindowFocus { pane }))
            .await?;
        for pressed in [true, false] {
            critical_send
                .send_client_message(&ClientMessage::Input(InputEvent::KeyboardKey {
                    keycode,
                    pressed,
                }))
                .await?;
        }
        let mut unchanged_scenes = 0_u8;
        let (input_scene, input_packets) = loop {
            let ServerMessage::Scene(next_scene) = latest_receive.receive_server_message().await?
            else {
                continue;
            };
            if next_scene.sequence <= scene.sequence || next_scene.windows.is_empty() {
                acknowledge_frame(&mut critical_send, next_scene.sequence).await?;
                continue;
            }
            let expected = next_scene
                .windows
                .iter()
                .map(|window| window.resource.id())
                .collect::<HashSet<_>>();
            let mut next_packets = Vec::with_capacity(expected.len());
            while next_packets.len() < expected.len() {
                let packet = frame_receive.receive_frame().await?;
                if packet.sequence == next_scene.sequence && expected.contains(&packet.resource) {
                    ensure!(
                        !packet.pixels()?.is_empty(),
                        "input response frame decompressed empty"
                    );
                    next_packets.push(packet);
                }
            }
            let changed = next_packets
                .iter()
                .find(|packet| packet.pane == pane)
                .context("input response scene did not include the focused pane")?
                .pixels()?
                != original_pixels;
            if !changed {
                unchanged_scenes = unchanged_scenes.saturating_add(1);
                ensure!(
                    unchanged_scenes <= 8,
                    "remote key transition did not change the focused pane"
                );
                acknowledge_frame(&mut critical_send, next_scene.sequence).await?;
                continue;
            }
            break (next_scene, next_packets);
        };
        acknowledge_frame(&mut critical_send, input_scene.sequence).await?;
        (input_scene.sequence, input_packets.len())
    } else {
        (scene.sequence, 0)
    };
    critical_send
        .send_client_message(&ClientMessage::Lifecycle(ClientLifecycle::Disconnect))
        .await?;
    critical_send.finish().await?;
    ensure!(
        critical_receive.receive_server_message().await? == ServerMessage::DisconnectAcknowledged,
        "remote relay did not acknowledge disconnect"
    );
    critical_receive.wait_for_finish().await?;
    println!(
        "OK remote transport generation={} catalog_revision={} sessions={} windows={} scene={} \
         input_scene={} frame_packets={} input_frame_packets={} compressed_bytes={} \
         backend_status_bytes={} artifacts_sent={} artifacts_received={}",
        network.connection_generation,
        catalog.revision,
        catalog.sessions.len(),
        catalog.windows.len(),
        scene.sequence,
        input_scene,
        packets.len(),
        input_frame_packets,
        packets
            .iter()
            .map(|packet| packet.compressed_pixels.len())
            .sum::<usize>(),
        backend_status.len(),
        artifacts_sent,
        artifacts_received
    );
    Ok(())
}

async fn probe_artifacts(
    send: &mut NetworkSendLane,
    receive: &mut NetworkReceiveLane,
    mut exchange: ArtifactExchange,
    expected_sent: usize,
    expected_received: usize,
) -> Result<(usize, usize)> {
    let mut outbound = VecDeque::from(exchange.initial_messages());
    let mut transfers = VecDeque::<ArtifactTransfer>::new();
    let mut accepted = HashSet::new();
    let mut installed = HashSet::new();
    while accepted.len() < expected_sent || installed.len() < expected_received {
        while let Some(message) = outbound.pop_front() {
            send.send_client_message(&ClientMessage::Artifact(message))
                .await?;
        }
        while let Some(transfer) = transfers.pop_front() {
            for message in transfer.messages()? {
                send.send_client_message(&ClientMessage::Artifact(message?))
                    .await?;
            }
        }
        let message = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            receive.receive_server_message(),
        )
        .await
        .context("timed out waiting for remote artifact exchange")??;
        let ServerMessage::Artifact(message) = message else {
            bail!("non-artifact server message reached the probe artifact lane");
        };
        let update = exchange.handle(message)?;
        for event in update.events {
            match event {
                ArtifactExchangeEvent::Installed(artifact) => {
                    outbound
                        .push_back(exchange.finish_installation(artifact.manifest.digest, Ok(()))?);
                    installed.insert(artifact.manifest.digest);
                    eprintln!(
                        "probe received client artifact id={} digest={}",
                        artifact.manifest.id,
                        digest_hex(artifact.manifest.digest)
                    );
                }
                ArtifactExchangeEvent::PeerAccepted(manifest) => {
                    accepted.insert(manifest.digest);
                    eprintln!(
                        "remote session accepted probe artifact id={} digest={}",
                        manifest.id,
                        digest_hex(manifest.digest)
                    );
                }
                ArtifactExchangeEvent::PeerRejected {
                    manifest,
                    digest,
                    reason,
                } => {
                    bail!(
                        "remote session rejected probe artifact id={} digest={} reason={reason}",
                        manifest
                            .as_ref()
                            .map_or("<unknown>", |value| value.id.as_str()),
                        digest_hex(digest)
                    );
                }
            }
        }
        outbound.extend(update.outbound);
        transfers.extend(update.transfers);
    }
    while let Some(message) = outbound.pop_front() {
        send.send_client_message(&ClientMessage::Artifact(message))
            .await?;
    }
    while let Some(transfer) = transfers.pop_front() {
        for message in transfer.messages()? {
            send.send_client_message(&ClientMessage::Artifact(message?))
                .await?;
        }
    }
    Ok((accepted.len(), installed.len()))
}

async fn acknowledge_frame(lane: &mut NetworkSendLane, sequence: u64) -> Result<()> {
    lane.send_client_message(&ClientMessage::Lifecycle(ClientLifecycle::FramePresented {
        sequence,
    }))
    .await
}
