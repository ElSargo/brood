use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    mem::size_of,
    net::{Ipv4Addr, SocketAddr},
    os::fd::AsFd,
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use brood_cli::control::{
    ControlTarget, activate_surface_session, close_surface_session,
    complete_realm_artifact_request, create_surface_session, handshake,
    install_session_init_artifact, open_presentation_fast_path, rename_surface_session,
    request_dbus_command, resolve_control_target, shared_memory_resource,
    take_realm_artifact_requests,
};
use brood_protocol::{
    ActorMessage, ArtifactDigest, ArtifactMessage, ArtifactRealm, BackendOperation, BackendRequest,
    BackendResponse, BackendResponseValue, CatalogSession, CatalogSnapshot, CatalogWindow,
    ClientLifecycle, ClientMessage, EndpointCapabilities, FrameResourceRef, FrameScene,
    FrameTransportKind, InputEvent, Lane, ServerMessage, SessionOperation, WindowFrame,
};
use brood_transport::{
    ArtifactExchange, ArtifactExchangeEvent, ArtifactOffer, NetworkFramePacket, NetworkReceiveLane,
    NetworkSendLane, accept_network_connection, client_endpoint, connect_network,
    default_artifact_cache, digest_file, digest_hex, package_offers, server_endpoint,
};
use clap::{Parser, Subcommand};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

mod ssh;

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
    critical: mpsc::UnboundedSender<CriticalOutbound>,
    reliable: mpsc::UnboundedSender<ServerMessage>,
    latest: mpsc::UnboundedSender<ServerMessage>,
    frames: mpsc::UnboundedSender<NetworkFramePacket>,
}

enum CriticalOutbound {
    Message(ServerMessage),
    DisconnectAcknowledgement(oneshot::Sender<()>),
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
        Command::Ssh(args) => ssh::deploy(&args),
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
    fs::write(key, signing_key.serialize_der())
        .with_context(|| format!("write remote presentation private key {}", key.display()))?;
    Ok(())
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
        critical: critical_tx.clone(),
        reliable: reliable_tx,
        latest: latest_tx,
        frames: frame_tx,
    };

    spawn_catalog_forwarder(
        &fast_path.catalog_fd,
        fast_path.catalog_notify_fd,
        fast_path.generation,
        network.backend,
        outbound.clone(),
    )?;
    spawn_frame_forwarder(
        target.clone(),
        &fast_path.frame_fd,
        fast_path.frame_notify_fd,
        fast_path.generation,
        network.backend,
        outbound.clone(),
    )?;
    spawn_actor_forwarder(
        &fast_path.presentation_message_fd,
        fast_path.presentation_message_notify_fd,
        fast_path.generation,
        outbound.clone(),
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
    let (bulk_send, bulk_receive) = network.take_lane(Lane::Bulk)?.split();
    let (frame_send, _frame_receive) = network.take_lane(Lane::FrameResource)?.split();

    tokio::spawn(send_critical_messages(critical_send, critical_rx));
    tokio::spawn(send_server_messages(reliable_send, reliable_rx));
    tokio::spawn(send_server_messages(latest_send, latest_rx));
    tokio::spawn(send_network_frames(frame_send, frame_rx));

    let (disconnect_tx, mut disconnect_rx) = oneshot::channel();
    let critical_input_tx = local_input_tx.clone();
    let disconnect_critical_tx = critical_tx;
    tokio::spawn(async move {
        loop {
            let message = critical_receive.receive_client_message().await?;
            let disconnect = message == ClientMessage::Lifecycle(ClientLifecycle::Disconnect);
            critical_input_tx
                .send(message)
                .map_err(|_error| anyhow!("local input writer stopped"))?;
            if disconnect {
                critical_receive.wait_for_finish().await?;
                let (acknowledged_tx, acknowledged_rx) = oneshot::channel();
                disconnect_critical_tx
                    .send(CriticalOutbound::DisconnectAcknowledgement(acknowledged_tx))
                    .map_err(|_error| anyhow!("critical network sender stopped"))?;
                acknowledged_rx
                    .await
                    .context("critical network sender dropped the disconnect acknowledgement")?;
                let _send_result = disconnect_tx.send(());
                return Ok::<(), anyhow::Error>(());
            }
        }
    });
    tokio::spawn(receive_latest_input(latest_receive, local_input_tx));
    tokio::spawn(receive_reliable_input(
        reliable_receive,
        local_actor_tx,
        target.clone(),
        outbound,
    ));
    let mut artifact_task = tokio::spawn(exchange_session_artifacts(
        bulk_send,
        bulk_receive,
        artifact_exchange,
        target,
        session_package,
        package_cache,
    ));

    let result = tokio::select! {
        disconnect = &mut disconnect_rx => {
            disconnect.context("remote client disconnected without a lifecycle message")
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
    artifact_task.abort();
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
    let mut waiting_by_digest = HashMap::<ArtifactDigest, Vec<u64>>::new();
    let mut active_package_roots = HashMap::<String, PathBuf>::new();
    let mut unresolved = VecDeque::<(u64, String, std::time::Instant)>::new();
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(100));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        while let Some(message) = outbound.pop_front() {
            send.send_server_message(&ServerMessage::Artifact(message))
                .await?;
        }

        tokio::select! {
            message = receive_client_artifact(&mut receive) => {
                let update = exchange.handle(message?)?;
                apply_session_artifact_update(
                    update,
                    &mut exchange,
                    &mut outbound,
                    &target,
                    &package_cache,
                    &mut waiting_by_digest,
                    &mut active_package_roots,
                ).await?;
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

async fn apply_session_artifact_update(
    update: brood_transport::ArtifactExchangeUpdate,
    exchange: &mut ArtifactExchange,
    outbound: &mut VecDeque<ArtifactMessage>,
    target: &ControlTarget,
    package_cache: &Path,
    waiting_by_digest: &mut HashMap<ArtifactDigest, Vec<u64>>,
    active_package_roots: &mut HashMap<String, PathBuf>,
) -> Result<()> {
    outbound.extend(update.outbound);
    for event in update.events {
        match event {
            ArtifactExchangeEvent::Installed(artifact) => {
                let digest = artifact.manifest.digest;
                let waiting = waiting_by_digest.remove(&digest).unwrap_or_default();
                let install_result = install_session_artifact(
                    target,
                    &artifact,
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
            let package_root = package_cache
                .join(&member.package)
                .join(digest_hex(artifact.manifest.digest));
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
        request_dbus_command(
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
    target: ControlTarget,
    outbound: Outbound,
) -> Result<()> {
    loop {
        match lane.receive_client_message().await? {
            ClientMessage::Actor(message) => sender
                .send(message)
                .map_err(|_error| anyhow!("local actor writer stopped"))?,
            ClientMessage::BackendRequest(request) => {
                let request_target = target.clone();
                let response = tokio::task::spawn_blocking(move || {
                    execute_backend_request(&request_target, request)
                })
                .await
                .context("join remote backend request")?;
                outbound
                    .reliable
                    .send(ServerMessage::BackendResponse(response))
                    .map_err(|_error| anyhow!("remote reliable sender stopped"))?;
            }
            ClientMessage::ReleaseResource { .. } => {}
            ClientMessage::Input(_) | ClientMessage::Lifecycle(_) | ClientMessage::Artifact(_) => {
                bail!("critical client message reached the reliable lane");
            }
        }
    }
}

fn execute_backend_request(target: &ControlTarget, request: BackendRequest) -> BackendResponse {
    let result = match request.operation {
        BackendOperation::Control { argv } => request_dbus_command(target, &argv, false)
            .map(BackendResponseValue::Control)
            .map_err(|error| format!("{error:#}")),
        BackendOperation::Session {
            operation,
            session,
            name,
            directory,
        } => execute_session_operation(target, operation, session, name, directory)
            .map(BackendResponseValue::Session)
            .map_err(|error| format!("{error:#}")),
    };
    BackendResponse {
        id: request.id,
        result,
    }
}

fn execute_session_operation(
    target: &ControlTarget,
    operation: SessionOperation,
    session: Option<u64>,
    name: Option<String>,
    directory: Option<String>,
) -> Result<Option<u64>> {
    match operation {
        SessionOperation::Refresh => Ok(None),
        SessionOperation::Activate => {
            let session = session.context("activate request is missing a session ID")?;
            ensure!(
                activate_surface_session(target, session)?,
                "session {session} is unavailable"
            );
            Ok(Some(session))
        }
        SessionOperation::Create => {
            if let Some(directory) = directory {
                let mut argv = vec![
                    "surface-session".to_string(),
                    "dir-new".to_string(),
                    directory,
                    "default".to_string(),
                ];
                if let Some(name) = name {
                    argv.push(name);
                }
                let response = request_dbus_command(target, &argv, false)?;
                let active = response
                    .split_whitespace()
                    .find_map(|part| part.strip_prefix("active=")?.parse().ok())
                    .context("remote directory session creation returned no active session")?;
                Ok(Some(active))
            } else {
                create_surface_session(target, name.unwrap_or_default()).map(Some)
            }
        }
        SessionOperation::Rename => {
            let session = session.context("rename request is missing a session ID")?;
            let name = name.context("rename request is missing a name")?;
            ensure!(
                rename_surface_session(target, session, name)?,
                "session {session} was not renamed"
            );
            Ok(Some(session))
        }
        SessionOperation::Close => {
            let session = session.context("close request is missing a session ID")?;
            ensure!(
                close_surface_session(target, session)?,
                "session {session} was not closed"
            );
            Ok(Some(session))
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

async fn send_critical_messages(
    mut lane: NetworkSendLane,
    mut receiver: mpsc::UnboundedReceiver<CriticalOutbound>,
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
                return Ok(());
            }
        }
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
            message.realm,
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
                                .send(ServerMessage::Catalog(CatalogSnapshot {
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
                                    backend,
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
                            if outbound
                                .critical
                                .send(CriticalOutbound::Message(message))
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
        }))
        .await?;

    let catalog = loop {
        if let ServerMessage::Catalog(catalog) = reliable_receive.receive_server_message().await? {
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
        if let ServerMessage::BackendResponse(response) =
            reliable_receive.receive_server_message().await?
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
    let mut accepted = HashSet::new();
    let mut installed = HashSet::new();
    while accepted.len() < expected_sent || installed.len() < expected_received {
        while let Some(message) = outbound.pop_front() {
            send.send_client_message(&ClientMessage::Artifact(message))
                .await?;
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
    }
    while let Some(message) = outbound.pop_front() {
        send.send_client_message(&ClientMessage::Artifact(message))
            .await?;
    }
    Ok((accepted.len(), installed.len()))
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
