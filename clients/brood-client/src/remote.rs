use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use brood_protocol::{
    ActorMessage, ArtifactMessage, ArtifactRealm, BackendId, BackendOperation, BackendRequest,
    BackendResponseValue, ClientLifecycle, ClientMessage, EndpointCapabilities, FramePixelFormat,
    FrameScene, InputEvent, Lane, ServerMessage,
};
use brood_transport::{
    ArtifactExchange, ArtifactExchangeEvent, ArtifactOffer, NetworkFramePacket, NetworkReceiveLane,
    NetworkSendLane, client_endpoint, connect_network, digest_hex,
};
use tokio::sync::mpsc as tokio_mpsc;
use winit::event_loop::EventLoopProxy;

use crate::{
    ClientEvent, FrameResource, PresentationWindowSummary, SceneWindowUpdate,
    actor_runtime::{BackendActorMessageRequest, SessionCommandEnvelope},
};
use brood_wasm::{
    SessionCommandCompletion, SessionOperation as WasmSessionOperation, SessionSnapshot,
    SessionSummary,
};

const INPUT_CAPACITY: usize = 512;
const RELIABLE_CAPACITY: usize = 128;
const MAX_PENDING_SCENES: usize = 4;

#[derive(Clone)]
pub struct RemoteConfig {
    pub backend: BackendId,
    pub connect: SocketAddr,
    pub server_name: String,
    pub server_certificate: PathBuf,
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
    pub artifact_offers: Vec<ArtifactOffer>,
    pub allowed_client_artifacts: Vec<String>,
    pub artifact_cache: PathBuf,
}

pub struct RemoteConnection {
    pub backend: BackendId,
    pub profile: brood_protocol::TransportProfile,
    pub generation: u64,
    input: Option<RemoteInputLane>,
    actor_messages: Option<RemoteActorMessageLane>,
    backend_commands: Option<RemoteBackendCommandLane>,
    shutdown_sender: tokio_mpsc::Sender<ClientMessage>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl RemoteConnection {
    pub fn start(config: RemoteConfig, proxy: &EventLoopProxy<ClientEvent>) -> Result<Self> {
        let configured_backend = config.backend;
        let (input_sender, input_receiver) = tokio_mpsc::channel(INPUT_CAPACITY);
        let (actor_sender, actor_receiver) = tokio_mpsc::channel(RELIABLE_CAPACITY);
        let (reliable_sender, reliable_receiver) = tokio_mpsc::channel(RELIABLE_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker_proxy = proxy.clone();
        let established = Arc::new(AtomicBool::new(false));
        let worker_established = Arc::clone(&established);
        let worker = thread::Builder::new()
            .name("brood-network-transport".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .thread_name("brood-network")
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .context("start remote presentation runtime")?;
                let result = runtime.block_on(run_connection(
                    config,
                    worker_proxy.clone(),
                    input_receiver,
                    actor_receiver,
                    reliable_receiver,
                    ready_sender,
                    worker_established,
                ));
                if let Err(error) = &result
                    && established.load(Ordering::Acquire)
                {
                    let _send_result = worker_proxy.send_event(ClientEvent::BackendDisconnected {
                        backend: configured_backend,
                        message: format!("remote presentation transport stopped: {error:#}"),
                    });
                }
                result
            })
            .context("spawn remote presentation transport")?;
        let ready = ready_receiver
            .recv()
            .context("remote presentation worker stopped before handshake")??;
        Ok(Self {
            backend: ready.backend,
            profile: ready.profile,
            generation: ready.generation,
            input: Some(RemoteInputLane::new(input_sender.clone())),
            actor_messages: Some(RemoteActorMessageLane::new(actor_sender)),
            backend_commands: Some(RemoteBackendCommandLane::new(reliable_sender)),
            shutdown_sender: input_sender,
            worker: Some(worker),
        })
    }

    pub fn take_lanes(
        &mut self,
    ) -> Result<(
        RemoteInputLane,
        RemoteActorMessageLane,
        RemoteBackendCommandLane,
    )> {
        Ok((
            self.input
                .take()
                .context("remote input lane was already taken")?,
            self.actor_messages
                .take()
                .context("remote actor-message lane was already taken")?,
            self.backend_commands
                .take()
                .context("remote backend-command lane was already taken")?,
        ))
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Err(error) = self
            .shutdown_sender
            .blocking_send(ClientMessage::Lifecycle(ClientLifecycle::Disconnect))
        {
            eprintln!("remote presentation disconnect channel already closed: {error}");
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_panic| anyhow!("remote presentation worker panicked"))??;
        }
        Ok(())
    }

    pub fn reap(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_panic| anyhow!("remote presentation worker panicked"))??;
        }
        Ok(())
    }
}

struct RemoteReady {
    backend: BackendId,
    profile: brood_protocol::TransportProfile,
    generation: u64,
}

async fn run_connection(
    config: RemoteConfig,
    proxy: EventLoopProxy<ClientEvent>,
    input_receiver: tokio_mpsc::Receiver<ClientMessage>,
    actor_receiver: tokio_mpsc::Receiver<ActorCommand>,
    reliable_receiver: tokio_mpsc::Receiver<ReliableCommand>,
    ready_sender: mpsc::SyncSender<Result<RemoteReady>>,
    established: Arc<AtomicBool>,
) -> Result<()> {
    let result = connect_and_split(&config).await;
    let (endpoint, mut network) = match result {
        Ok(connection) => connection,
        Err(error) => {
            let message = format!("{error:#}");
            let _send_result = ready_sender.send(Err(error));
            return Err(anyhow!(message));
        }
    };
    let profile = network.profile;
    let backend = network.backend;
    let generation = network.connection_generation;
    let (critical_send, critical_receive) = network.take_lane(Lane::Critical)?.split();
    let (session0_send, session0_receive) = network.take_lane(Lane::InteractiveSession0)?.split();
    let (session1_send, session1_receive) = network.take_lane(Lane::InteractiveSession1)?.split();
    let (session2_send, session2_receive) = network.take_lane(Lane::InteractiveSession2)?.split();
    let (session3_send, session3_receive) = network.take_lane(Lane::InteractiveSession3)?.split();
    let (daemon_send, daemon_receive) = network.take_lane(Lane::InteractiveDaemon)?.split();
    let (reliable_send, reliable_receive) = network.take_lane(Lane::Reliable)?.split();
    let (latest_send, latest_receive) = network.take_lane(Lane::LatestState)?.split();
    let (bulk_send, bulk_receive) = network.take_lane(Lane::Bulk)?.split();
    let (_frame_send, frame_receive) = network.take_lane(Lane::FrameResource)?.split();
    ready_sender
        .send(Ok(RemoteReady {
            backend,
            profile,
            generation,
        }))
        .map_err(|_error| anyhow!("remote presentation startup receiver stopped"))?;
    established.store(true, Ordering::Release);

    let disconnect_acknowledged = Arc::new(tokio::sync::Notify::new());
    let mut input_task = tokio::spawn(send_input_messages(
        critical_send,
        latest_send,
        input_receiver,
        Arc::clone(&disconnect_acknowledged),
    ));
    let mut critical_task = tokio::spawn(receive_critical_messages(
        critical_receive,
        proxy.clone(),
        disconnect_acknowledged,
    ));
    let mut actor_send_task = tokio::spawn(send_actor_messages(
        [session0_send, session1_send, session2_send, session3_send],
        daemon_send,
        actor_receiver,
        proxy.clone(),
        backend,
    ));
    let mut actor_receive_task = tokio::spawn(receive_actor_messages_many(
        vec![
            session0_receive,
            session1_receive,
            session2_receive,
            session3_receive,
        ],
        proxy.clone(),
        backend,
    ));
    let mut daemon_receive_task = tokio::spawn(receive_actor_messages(
        daemon_receive,
        proxy.clone(),
        backend,
    ));
    let mut reliable_task = tokio::spawn(exchange_reliable_messages(
        reliable_send,
        reliable_receive,
        reliable_receiver,
        proxy.clone(),
        backend,
    ));
    let mut frame_task = tokio::spawn(receive_scenes(
        latest_receive,
        frame_receive,
        proxy.clone(),
        backend,
    ));
    let artifact_exchange = ArtifactExchange::new(
        ArtifactRealm::Client,
        config.allowed_client_artifacts,
        config.artifact_cache,
        config.artifact_offers,
    )?;
    let mut artifact_task = tokio::spawn(exchange_artifacts(
        bulk_send,
        bulk_receive,
        artifact_exchange,
        proxy,
    ));

    let result = tokio::select! {
        biased;
        result = &mut input_task => join_transport_task("input", result),
        result = &mut critical_task => join_transport_task("critical", result),
        result = &mut actor_send_task => join_transport_task("actor send", result),
        result = &mut actor_receive_task => join_transport_task("actor receive", result),
        result = &mut daemon_receive_task => join_transport_task("daemon actor receive", result),
        result = &mut reliable_task => join_transport_task("reliable", result),
        result = &mut frame_task => join_transport_task("frame", result),
        result = &mut artifact_task => join_transport_task("artifact", result),
    };
    input_task.abort();
    critical_task.abort();
    actor_send_task.abort();
    actor_receive_task.abort();
    daemon_receive_task.abort();
    reliable_task.abort();
    frame_task.abort();
    artifact_task.abort();
    let diagnostics = network.diagnostics();
    eprintln!(
        "remote client QUIC diagnostics {{\"rtt_us\":{},\"congestion_window\":{},\"congestion_events\":{},\"lost_packets\":{},\"lost_bytes\":{},\"sent_packets\":{},\"mtu\":{}}}",
        diagnostics.rtt_us,
        diagnostics.congestion_window,
        diagnostics.congestion_events,
        diagnostics.lost_packets,
        diagnostics.lost_bytes,
        diagnostics.sent_packets,
        diagnostics.mtu,
    );
    drop(network);
    drop(endpoint);
    result
}

async fn connect_and_split(
    config: &RemoteConfig,
) -> Result<(quinn::Endpoint, brood_transport::NetworkConnection)> {
    let endpoint = client_endpoint(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        fs::read(&config.server_certificate).with_context(|| {
            format!(
                "read remote server certificate {}",
                config.server_certificate.display()
            )
        })?,
        fs::read(&config.client_certificate).with_context(|| {
            format!(
                "read remote client certificate {}",
                config.client_certificate.display()
            )
        })?,
        fs::read(&config.client_private_key).with_context(|| {
            format!(
                "read remote client private key {}",
                config.client_private_key.display()
            )
        })?,
    )?;
    let network = connect_network(
        &endpoint,
        config.connect,
        &config.server_name,
        config.backend,
        EndpointCapabilities::remote(),
    )
    .await?;
    Ok((endpoint, network))
}

fn join_transport_task(
    name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    result
        .with_context(|| format!("{name} presentation task panicked"))?
        .with_context(|| format!("{name} presentation task stopped"))
}

async fn send_input_messages(
    mut critical: NetworkSendLane,
    mut latest: NetworkSendLane,
    mut receiver: tokio_mpsc::Receiver<ClientMessage>,
    disconnect_acknowledged: Arc<tokio::sync::Notify>,
) -> Result<()> {
    while let Some(message) = receiver.recv().await {
        let disconnect = message == ClientMessage::Lifecycle(ClientLifecycle::Disconnect);
        match &message {
            ClientMessage::Input(input) if input.lane() == Lane::LatestState => {
                latest.send_client_message(&message).await?;
            }
            ClientMessage::Input(_) | ClientMessage::Lifecycle(_) => {
                critical.send_client_message(&message).await?;
            }
            ClientMessage::Actor(_)
            | ClientMessage::BackendRequest(_)
            | ClientMessage::Artifact(_)
            | ClientMessage::ReleaseResource { .. } => {
                bail!("reliable client message reached the input sender");
            }
        }
        if disconnect {
            critical.finish().await?;
            disconnect_acknowledged.notified().await;
            return Ok(());
        }
    }
    Ok(())
}

async fn receive_critical_messages(
    mut lane: NetworkReceiveLane,
    proxy: EventLoopProxy<ClientEvent>,
    disconnect_acknowledged: Arc<tokio::sync::Notify>,
) -> Result<()> {
    loop {
        let event = match lane.receive_server_message().await? {
            ServerMessage::FocusPresentation => ClientEvent::FocusPresentation,
            ServerMessage::ClosePresentation => ClientEvent::ClosePresentation,
            ServerMessage::DisconnectAcknowledged => {
                lane.wait_for_finish().await?;
                disconnect_acknowledged.notify_one();
                return Ok(());
            }
            ServerMessage::Catalog(_)
            | ServerMessage::Scene(_)
            | ServerMessage::Actor(_)
            | ServerMessage::BackendResponse(_)
            | ServerMessage::Artifact(_) => {
                bail!("non-critical server message reached the critical lane");
            }
        };
        proxy
            .send_event(event)
            .map_err(|_error| anyhow!("presentation event loop stopped"))?;
    }
}

struct ActorCommand {
    message: ActorMessage,
    queued_at: Instant,
}

enum ReliableCommand {
    Backend {
        request: BackendRequest,
        context: PendingBackend,
    },
}

enum PendingBackend {
    Control {
        description: String,
    },
    Session {
        actor_id: String,
        request: brood_wasm::SessionCommandRequest,
    },
}

async fn send_actor_messages(
    session_sends: [NetworkSendLane; 4],
    daemon_send: NetworkSendLane,
    mut outbound: tokio_mpsc::Receiver<ActorCommand>,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    let mut senders = Vec::with_capacity(5);
    let mut tasks = tokio::task::JoinSet::new();
    for send in session_sends {
        let (sender, receiver) = tokio_mpsc::channel(RELIABLE_CAPACITY);
        senders.push(sender);
        tasks.spawn(send_actor_lane(send, receiver, proxy.clone(), backend));
    }
    let (daemon_sender, daemon_receiver) = tokio_mpsc::channel(RELIABLE_CAPACITY);
    senders.push(daemon_sender);
    tasks.spawn(send_actor_lane(
        daemon_send,
        daemon_receiver,
        proxy,
        backend,
    ));

    loop {
        tokio::select! {
            completed = tasks.join_next(), if !tasks.is_empty() => {
                return completed
                    .context("interactive actor sender set is empty")?
                    .context("interactive actor sender panicked")?;
            }
            command = outbound.recv() => {
                let Some(command) = command else {
                    break;
                };
                let lane = match command.message.realm {
                    brood_protocol::BackendActorRealm::Daemon => 4,
                    brood_protocol::BackendActorRealm::Session(session) => {
                        usize::try_from(session % 4).unwrap_or_default()
                    }
                };
                senders[lane]
                    .try_send(command)
                    .map_err(|_error| anyhow!("interactive actor sender is full or stopped"))?;
            }
        }
    }
    drop(senders);
    while let Some(completed) = tasks.join_next().await {
        completed.context("interactive actor sender panicked")??;
    }
    Ok(())
}

async fn send_actor_lane(
    mut send: NetworkSendLane,
    mut outbound: tokio_mpsc::Receiver<ActorCommand>,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    while let Some(ActorCommand { message, queued_at }) = outbound.recv().await {
        let terminal_input = message.sender.starts_with("terminal-ui")
            && message.recipient.starts_with("terminal-session")
            && crate::terminal_input_sequence(&message.body).is_some();
        let trace_sequence = crate::terminal_input_sequence(&message.body);
        send.send_client_message(&ClientMessage::Actor(message))
            .await?;
        if terminal_input && let Some(trace_sequence) = trace_sequence {
            proxy
                .send_event(ClientEvent::RemoteTerminalInputSent {
                    backend,
                    trace_sequence,
                    queued_at,
                    sent_at: Instant::now(),
                })
                .map_err(|_error| anyhow!("presentation event loop stopped"))?;
        }
    }
    Ok(())
}

async fn receive_actor_messages_many(
    receives: Vec<NetworkReceiveLane>,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for receive in receives {
        tasks.spawn(receive_actor_messages(receive, proxy.clone(), backend));
    }
    let result = tasks
        .join_next()
        .await
        .context("interactive actor receiver set is empty")?;
    tasks.abort_all();
    result.context("interactive actor receiver panicked")?
}

async fn receive_actor_messages(
    mut receive: NetworkReceiveLane,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    loop {
        let ServerMessage::Actor(message) = receive.receive_server_message().await? else {
            bail!("non-actor server message reached the interactive lane");
        };
        proxy
            .send_event(ClientEvent::BackendActorMessage {
                backend,
                realm: message.realm,
                sender: message.sender,
                recipient: message.recipient,
                body: message.body,
                received_at: Instant::now(),
            })
            .map_err(|_error| anyhow!("presentation event loop stopped"))?;
    }
}

async fn exchange_reliable_messages(
    mut send: NetworkSendLane,
    receive: NetworkReceiveLane,
    mut outbound: tokio_mpsc::Receiver<ReliableCommand>,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    let (incoming_sender, mut incoming) = tokio_mpsc::channel(32);
    let receive_task = tokio::spawn(receive_reliable_messages(receive, incoming_sender));
    let mut pending = HashMap::<u64, PendingBackend>::new();
    let result = loop {
        tokio::select! {
            command = outbound.recv() => {
                let Some(command) = command else {
                    break Ok(());
                };
                match command {
                    ReliableCommand::Backend { request, context } => {
                        ensure!(
                            pending.insert(request.id, context).is_none(),
                            "duplicate remote backend request ID"
                        );
                        if let Err(error) = send
                            .send_client_message(&ClientMessage::BackendRequest(request.clone()))
                            .await
                        {
                            pending.remove(&request.id);
                            return Err(error);
                        }
                    }
                }
            }
            message = incoming.recv() => {
                let Some(message) = message else {
                    break Err(anyhow!("remote reliable receive task stopped"));
                };
        handle_reliable_message(message?, &mut pending, &proxy, backend)?;
            }
        }
    };
    receive_task.abort();
    result
}

async fn receive_reliable_messages(
    mut receive: NetworkReceiveLane,
    sender: tokio_mpsc::Sender<Result<ServerMessage>>,
) {
    loop {
        let message = receive
            .receive_server_message()
            .await
            .context("receive reliable presentation message");
        let stopped = message.is_err();
        if sender.send(message).await.is_err() || stopped {
            return;
        }
    }
}

fn handle_reliable_message(
    message: ServerMessage,
    pending: &mut HashMap<u64, PendingBackend>,
    proxy: &EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    let event = match message {
        ServerMessage::Catalog(catalog) => {
            ensure!(
                catalog.backend == backend,
                "remote catalog backend identity does not match its connection"
            );
            let sessions = catalog
                .sessions
                .into_iter()
                .map(|session| SessionSummary {
                    id: session.id,
                    name: session.name,
                    active: session.active,
                    windows: session.windows,
                    working_directory: session.working_directory,
                    project_id: session.project_id,
                    project_name: session.project_name,
                    project_directory: session.project_directory,
                    branch: session.branch,
                    worktree: session.worktree,
                    git: session.git,
                })
                .collect();
            let windows = catalog
                .windows
                .into_iter()
                .map(|window| PresentationWindowSummary {
                    pane: window.pane,
                    session: window.session,
                    app_id: window.app_id,
                })
                .collect();
            ClientEvent::BackendSnapshot {
                backend,
                session: Some(SessionSnapshot {
                    backend_id: backend.0,
                    revision: catalog.revision,
                    active: catalog.active_session,
                    sessions,
                }),
                active_session: catalog.active_session,
                windows,
            }
        }
        ServerMessage::Actor(_) => bail!("actor message reached the reliable lane"),
        ServerMessage::BackendResponse(response) => {
            let context = pending
                .remove(&response.id)
                .context("received response for an unknown remote backend request")?;
            match context {
                PendingBackend::Control { description } => {
                    let result = response.result.and_then(|value| match value {
                        BackendResponseValue::Control(response) => Ok(response),
                        BackendResponseValue::Session(_) => Err(
                            "remote backend returned a session value for a control request"
                                .to_string(),
                        ),
                    });
                    ClientEvent::BackendCommandCompleted {
                        description,
                        result,
                    }
                }
                PendingBackend::Session { actor_id, request } => {
                    let (success, session, message) = match response.result {
                        Ok(BackendResponseValue::Session(session)) => {
                            (true, session, String::new())
                        }
                        Ok(BackendResponseValue::Control(_)) => (
                            false,
                            None,
                            "remote backend returned a control value for a session request"
                                .to_string(),
                        ),
                        Err(message) => (false, None, message),
                    };
                    ClientEvent::SessionCommandCompleted {
                        actor_id,
                        completion: SessionCommandCompletion {
                            backend_id: backend.0,
                            request: request.request,
                            operation: request.operation,
                            success,
                            session,
                            message,
                        },
                    }
                }
            }
        }
        ServerMessage::Scene(_)
        | ServerMessage::Artifact(_)
        | ServerMessage::FocusPresentation
        | ServerMessage::ClosePresentation
        | ServerMessage::DisconnectAcknowledged => {
            bail!("non-reliable server message reached the reliable lane");
        }
    };
    proxy
        .send_event(event)
        .map_err(|_error| anyhow!("presentation event loop stopped"))
}

async fn exchange_artifacts(
    mut send: NetworkSendLane,
    mut receive: NetworkReceiveLane,
    mut exchange: ArtifactExchange,
    proxy: EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let mut outbound = VecDeque::from(exchange.initial_messages());
    loop {
        while let Some(message) = outbound.pop_front() {
            send.send_client_message(&ClientMessage::Artifact(message))
                .await?;
        }
        let message = receive_server_artifact(&mut receive).await?;
        let update = exchange.handle(message)?;
        outbound.extend(update.outbound);
        for event in update.events {
            match event {
                ArtifactExchangeEvent::Installed(artifact) => {
                    let digest = artifact.manifest.digest;
                    let (completion, installed) = tokio::sync::oneshot::channel();
                    proxy
                        .send_event(ClientEvent::RemoteArtifactInstalled {
                            manifest: artifact.manifest,
                            path: artifact.path,
                            completion,
                        })
                        .map_err(|_error| anyhow!("presentation event loop stopped"))?;
                    let result = installed
                        .await
                        .context("presentation event loop dropped artifact installation")?;
                    outbound.push_back(exchange.finish_installation(digest, result)?);
                }
                ArtifactExchangeEvent::PeerAccepted(manifest) => {
                    eprintln!(
                        "remote session accepted artifact id={} digest={}",
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
                        "remote session rejected artifact id={} digest={} reason={reason}",
                        manifest
                            .as_ref()
                            .map_or("<unknown>", |value| value.id.as_str()),
                        digest_hex(digest)
                    );
                }
            }
        }
    }
}

async fn receive_server_artifact(lane: &mut NetworkReceiveLane) -> Result<ArtifactMessage> {
    let message = lane.receive_server_message().await?;
    let ServerMessage::Artifact(message) = message else {
        bail!("non-artifact server message reached the artifact lane");
    };
    Ok(message)
}

async fn receive_scenes(
    latest: NetworkReceiveLane,
    frames: NetworkReceiveLane,
    proxy: EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    let (sender, mut events) = tokio_mpsc::channel(16);
    let scene_task = tokio::spawn(receive_scene_messages(latest, sender.clone()));
    let frame_task = tokio::spawn(receive_frame_messages(frames, sender));
    let mut pending_scenes = BTreeMap::<u64, FrameScene>::new();
    let mut pending_frames = HashMap::<(u64, u64), NetworkFramePacket>::new();
    let result = loop {
        let Some(event) = events.recv().await else {
            break Err(anyhow!("remote scene receive tasks stopped"));
        };
        match event? {
            RemoteSceneEvent::Scene(scene) => {
                pending_scenes.insert(scene.sequence, scene);
                while pending_scenes.len() > MAX_PENDING_SCENES {
                    let Some(sequence) = pending_scenes.keys().next().copied() else {
                        break;
                    };
                    pending_scenes.remove(&sequence);
                    pending_frames.retain(|(frame_sequence, _), _| *frame_sequence > sequence);
                }
            }
            RemoteSceneEvent::Frame(frame) => {
                pending_frames.insert((frame.sequence, frame.resource), frame);
            }
        }
        publish_ready_scenes(&mut pending_scenes, &mut pending_frames, &proxy, backend)?;
    };
    scene_task.abort();
    frame_task.abort();
    result
}

enum RemoteSceneEvent {
    Scene(FrameScene),
    Frame(NetworkFramePacket),
}

async fn receive_scene_messages(
    mut lane: NetworkReceiveLane,
    sender: tokio_mpsc::Sender<Result<RemoteSceneEvent>>,
) {
    loop {
        let result = lane.receive_server_message().await.and_then(|message| {
            let ServerMessage::Scene(scene) = message else {
                bail!("non-scene server message reached the latest-state lane");
            };
            Ok(RemoteSceneEvent::Scene(scene))
        });
        let stopped = result.is_err();
        if sender.send(result).await.is_err() || stopped {
            return;
        }
    }
}

async fn receive_frame_messages(
    mut lane: NetworkReceiveLane,
    sender: tokio_mpsc::Sender<Result<RemoteSceneEvent>>,
) {
    loop {
        let result = lane
            .receive_frame()
            .await
            .map(RemoteSceneEvent::Frame)
            .context("receive presentation frame resource");
        let stopped = result.is_err();
        if sender.send(result).await.is_err() || stopped {
            return;
        }
    }
}

fn publish_ready_scenes(
    scenes: &mut BTreeMap<u64, FrameScene>,
    frames: &mut HashMap<(u64, u64), NetworkFramePacket>,
    proxy: &EventLoopProxy<ClientEvent>,
    backend: BackendId,
) -> Result<()> {
    loop {
        let Some((&sequence, scene)) = scenes.first_key_value() else {
            return Ok(());
        };
        if !scene
            .windows
            .iter()
            .all(|window| frames.contains_key(&(sequence, window.resource.id())))
        {
            return Ok(());
        }
        let scene = scenes
            .remove(&sequence)
            .context("ready remote scene disappeared")?;
        ensure!(
            scene.backend == backend,
            "remote scene backend identity does not match its connection"
        );
        let mut windows = Vec::with_capacity(scene.windows.len());
        for window in scene.windows {
            let resource = window.resource.id();
            let packet = frames
                .remove(&(sequence, resource))
                .context("ready remote frame disappeared")?;
            ensure!(
                packet.sequence == sequence
                    && packet.resource == resource
                    && packet.pane == window.pane,
                "remote frame identity does not match its scene"
            );
            let pixels = packet.pixels()?;
            windows.push(SceneWindowUpdate {
                backend,
                window_id: window.pane,
                resource_id: resource,
                rect: window.source,
                active: window.active,
                resource: Some(FrameResource::Network(NetworkFrame {
                    width: packet.width,
                    height: packet.height,
                    stride: packet.stride,
                    pixel_format: packet.pixel_format,
                    color_space: packet.color_space,
                    compressed_bytes: packet.compressed_pixels.len(),
                    pixels,
                })),
            });
        }
        proxy
            .send_event(ClientEvent::SceneFrame {
                backend,
                sequence,
                width: scene.extent[0],
                height: scene.extent[1],
                windows,
            })
            .map_err(|_error| anyhow!("presentation event loop stopped"))?;
        frames.retain(|(frame_sequence, _), _| *frame_sequence > sequence);
    }
}

#[derive(Debug)]
pub struct NetworkFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: FramePixelFormat,
    pub color_space: brood_protocol::FrameColorSpace,
    pub compressed_bytes: usize,
    pub pixels: Vec<u8>,
}

pub struct RemoteInputLane {
    sender: tokio_mpsc::Sender<ClientMessage>,
    deferred: VecDeque<ClientMessage>,
    deferred_high_water: usize,
    coalesced_motion: u64,
    pane_routes: BTreeMap<u64, u64>,
}

impl RemoteInputLane {
    const fn new(sender: tokio_mpsc::Sender<ClientMessage>) -> Self {
        Self {
            sender,
            deferred: VecDeque::new(),
            deferred_high_water: 0,
            coalesced_motion: 0,
            pane_routes: BTreeMap::new(),
        }
    }

    pub fn set_pane_routes(&mut self, routes: impl IntoIterator<Item = (u64, u64)>) {
        self.pane_routes = routes.into_iter().collect();
    }

    pub fn pane_routes(&self) -> BTreeMap<u64, u64> {
        self.pane_routes.clone()
    }

    pub fn send(&mut self, message: ClientMessage) -> bool {
        self.flush_deferred();
        if self.has_deferred() {
            self.defer(message);
            return true;
        }
        match self.sender.try_send(message) {
            Ok(()) => true,
            Err(tokio_mpsc::error::TrySendError::Full(message)) => {
                self.defer(message);
                true
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn flush_deferred(&mut self) {
        let had_deferred = !self.deferred.is_empty();
        while let Some(message) = self.deferred.pop_front() {
            match self.sender.try_send(message) {
                Ok(()) => {}
                Err(tokio_mpsc::error::TrySendError::Full(message)) => {
                    self.deferred.push_front(message);
                    break;
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                    self.deferred.clear();
                    break;
                }
            }
        }
        if had_deferred && self.deferred.is_empty() {
            eprintln!(
                "drained deferred network input high_water={} coalesced_motion={}",
                self.deferred_high_water, self.coalesced_motion
            );
            self.deferred_high_water = 0;
            self.coalesced_motion = 0;
        }
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    fn defer(&mut self, message: ClientMessage) {
        if is_pointer_motion(&message)
            && let Some(previous) = self.deferred.back_mut()
            && is_pointer_motion(previous)
        {
            *previous = message;
            self.coalesced_motion = self.coalesced_motion.saturating_add(1);
            return;
        }
        if self.deferred.is_empty() {
            eprintln!("network input lane backpressured; deferring input messages");
        }
        self.deferred.push_back(message);
        self.deferred_high_water = self.deferred_high_water.max(self.deferred.len());
    }
}

const fn is_pointer_motion(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::Input(InputEvent::PointerMotion { .. })
    )
}

pub struct RemoteActorMessageLane {
    sender: tokio_mpsc::Sender<ActorCommand>,
    deferred: VecDeque<ActorCommand>,
}

impl RemoteActorMessageLane {
    const fn new(sender: tokio_mpsc::Sender<ActorCommand>) -> Self {
        Self {
            sender,
            deferred: VecDeque::new(),
        }
    }

    pub fn send(&mut self, message: &BackendActorMessageRequest) -> Result<()> {
        self.flush_deferred();
        let command = ActorCommand {
            message: ActorMessage {
                realm: message.realm,
                sender: message.sender.clone(),
                recipient: message.actor.clone(),
                body: message.body.clone(),
            },
            queued_at: Instant::now(),
        };
        if self.has_deferred() {
            return self.defer(command);
        }
        match self.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(tokio_mpsc::error::TrySendError::Full(command)) => self.defer(command),
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                bail!("remote actor-message channel is closed")
            }
        }
    }

    pub fn flush_deferred(&mut self) {
        while let Some(command) = self.deferred.pop_front() {
            match self.sender.try_send(command) {
                Ok(()) => {}
                Err(tokio_mpsc::error::TrySendError::Full(command)) => {
                    self.deferred.push_front(command);
                    break;
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                    self.deferred.clear();
                    break;
                }
            }
        }
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    fn defer(&mut self, command: ActorCommand) -> Result<()> {
        ensure!(
            self.deferred.len() < 64,
            "remote actor-message backlog is full"
        );
        self.deferred.push_back(command);
        Ok(())
    }
}

#[derive(Clone)]
pub struct RemoteBackendCommandLane {
    sender: tokio_mpsc::Sender<ReliableCommand>,
    next_request: Arc<AtomicU64>,
}

impl RemoteBackendCommandLane {
    fn new(sender: tokio_mpsc::Sender<ReliableCommand>) -> Self {
        Self {
            sender,
            next_request: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn try_send(&self, description: String, argv: Vec<String>) -> Result<()> {
        self.try_send_command(
            BackendOperation::Control { argv },
            PendingBackend::Control { description },
        )
    }

    pub fn try_send_session(&self, command: SessionCommandEnvelope) -> Result<()> {
        let operation = match command.request.operation {
            WasmSessionOperation::Refresh => brood_protocol::SessionOperation::Refresh,
            WasmSessionOperation::Activate => brood_protocol::SessionOperation::Activate,
            WasmSessionOperation::Create => brood_protocol::SessionOperation::Create,
            WasmSessionOperation::Rename => brood_protocol::SessionOperation::Rename,
            WasmSessionOperation::Close => brood_protocol::SessionOperation::Close,
        };
        self.try_send_command(
            BackendOperation::Session {
                operation,
                session: command.request.session,
                name: command.request.name.clone(),
                directory: command.request.directory.clone(),
            },
            PendingBackend::Session {
                actor_id: command.actor_id,
                request: command.request,
            },
        )
    }

    fn try_send_command(&self, operation: BackendOperation, context: PendingBackend) -> Result<()> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.sender
            .try_send(ReliableCommand::Backend {
                request: BackendRequest { id, operation },
                context,
            })
            .context("remote backend command queue is full or closed")
    }
}
