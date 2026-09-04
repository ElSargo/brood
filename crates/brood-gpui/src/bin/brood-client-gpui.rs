// GPUI-owned local presentation client.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::clone_on_ref_ptr,
    clippy::default_trait_access,
    clippy::indexing_slicing,
    clippy::let_underscore_must_use,
    clippy::map_entry,
    clippy::map_err_ignore,
    clippy::missing_const_for_fn,
    clippy::needless_pass_by_value,
    clippy::no_effect_underscore_binding,
    clippy::option_if_let_else,
    clippy::redundant_clone,
    clippy::result_large_err,
    clippy::semicolon_if_nothing_returned,
    clippy::significant_drop_in_scrutinee,
    clippy::significant_drop_tightening,
    clippy::too_many_arguments,
    reason = "the client bridges bounded wire values, thread-affine GPUI callbacks, and retained transport lanes"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead as _, BufReader},
    mem::size_of,
    net::ToSocketAddrs as _,
    os::fd::AsFd as _,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use crate::{
    ActionSink, ActorInputEvent, CopiedPane, ExternalPane, LinuxSurfaceDescriptor, ManagedUiRoot,
    PaneGeometry, PresentationCommand, PresentationHandle, drive_presentation,
    presentation_channel,
};
use anyhow::{Context as _, Result, bail};
use brood_cli::{
    ClientArgs,
    control::{
        ServerLaunch, connect_or_spawn, dmabuf_resource, open_presentation_fast_path,
        resolve_control_target, shared_memory_resource,
    },
};
use brood_keybindings::{ACTIONS, Binding, action};
use brood_protocol::{
    BackendActorRealm, ClientLifecycle, ClientMessage, FramePixelFormat, FrameTransportKind,
};
use brood_wasm::{
    ABI_INPUT_MODE_PASSTHROUGH, ABI_KEY_PRESSED, ABI_KEY_RELEASED, ActorFrameDraw, ComponentActor,
    ComponentActorConfig, ComponentActorEvent, ComponentChildEvent, FilesystemGrant,
    GlobalKeyDecision, GlobalKeyPolicyInput, GlobalKeyPolicyInvocation, HostMessage, InteractionId,
    LegacyWasmActorGeneration, LegacyWasmRuntimeEvent, Mailbox, MailboxAddress,
    PluginLifecycleEntry, PluginLifecycleEvent, PluginStartContext, PluginStartReason,
    PointerCapture, PointerDelivery, PointerPhase, PointerPolicyInvocation, PointerPolicyQuery,
    PresentationError, PresentationEvent, PresentationHost, PurePaneOutput, PureRenderError,
    PureRenderRequest, Registry, RenderHint, RenderPaneTarget, RenderToken, RuntimeIoCapabilities,
    RuntimeIoConfig, SessionCommandCompletion, SessionCommandRequest, SessionEvent,
    SessionOperation, SessionSnapshot, SessionSummary, TextLayoutOptions, TextLayoutResult,
    WasmActor, WasmActorDeclaration, WasmActorEventKind, WasmGlobalKeyPolicy, WasmPluginArtifact,
    WasmPointerPolicy,
};
use clap::Parser as _;
use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};
use serde::{Deserialize, Serialize};

#[path = "../client_remote.rs"]
mod remote;

#[derive(Clone)]
struct EventProxy(mpsc::SyncSender<ClientEvent>);

impl EventProxy {
    fn send_event(
        &self,
        event: ClientEvent,
    ) -> std::result::Result<(), mpsc::SendError<ClientEvent>> {
        self.0.send(event)
    }
}

#[derive(Clone)]
struct DynamicBackendHandle {
    sender: mpsc::SyncSender<DynamicBackendRequest>,
    can_connect_remote: bool,
    deployer: Option<PathBuf>,
}

impl DynamicBackendHandle {
    fn connect(&self, address: String) -> Result<()> {
        anyhow::ensure!(
            self.can_connect_remote,
            "dynamic remote transport credentials are unavailable"
        );
        self.sender
            .try_send(DynamicBackendRequest::Connect(address))
            .map_err(|error| anyhow::anyhow!("dynamic backend queue rejected request: {error}"))
    }

    fn deploy_ssh(&self, target: String) -> Result<()> {
        let deployer = self
            .deployer
            .clone()
            .context("the brood-remote deployer is unavailable")?;
        spawn_backend_deployment(
            deployer,
            DeploymentKind::Ssh { target },
            self.sender.clone(),
        )
    }

    fn deploy_container(&self, image: String, connection: Option<String>) -> Result<()> {
        let deployer = self
            .deployer
            .clone()
            .context("the brood-remote deployer is unavailable")?;
        spawn_backend_deployment(
            deployer,
            DeploymentKind::Container { image, connection },
            self.sender.clone(),
        )
    }
}

enum DynamicBackendRequest {
    Connect(String),
    Attach {
        config: Box<remote::RemoteConfig>,
        deployment: BackendDeployment,
    },
}

#[derive(Debug)]
enum DeploymentKind {
    Ssh {
        target: String,
    },
    Container {
        image: String,
        connection: Option<String>,
    },
}

struct BackendDeployment {
    description: String,
    child: Child,
    stdin: Option<ChildStdin>,
}

#[derive(Deserialize)]
struct BackendAttachReady {
    connect: std::net::SocketAddr,
    server_name: String,
    server_certificate: PathBuf,
    client_certificate: PathBuf,
    client_private_key: PathBuf,
    session_package: String,
    session_root: PathBuf,
}

struct PresentationWindowSummary {
    pane: u64,
    session: u64,
    app_id: String,
    size_authority: brood_protocol::ManagedWindowSizeAuthority,
    automation: brood_protocol::ManagedWindowAutomationState,
}

#[derive(Debug)]
struct SceneWindowUpdate {
    backend: brood_protocol::BackendId,
    backend_window_id: u64,
    window_id: u64,
    resource_id: u64,
    rect: [i32; 4],
    active: bool,
    resize_epoch: Option<u64>,
    resource: Option<FrameResource>,
}

#[derive(Debug)]
enum FrameResource {
    Network(remote::NetworkFrame),
}

enum ClientEvent {
    SceneFrame {
        backend: brood_protocol::BackendId,
        connection_generation: u64,
        sequence: u64,
        width: u32,
        height: u32,
        windows: Vec<SceneWindowUpdate>,
    },
    ManagedResizeStatus {
        backend: brood_protocol::BackendId,
        connection_generation: u64,
        status: brood_protocol::ManagedResizeStatus,
    },
    FocusPresentation,
    ClosePresentation,
    BackendDisconnected {
        backend: brood_protocol::BackendId,
        message: String,
    },
    RemoteArtifactInstalled {
        manifest: brood_protocol::ArtifactManifest,
        path: std::path::PathBuf,
        completion: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    },
    BackendCommandCompleted {
        description: String,
        result: std::result::Result<String, String>,
    },
    BackendSnapshot {
        backend: brood_protocol::BackendId,
        session: Option<SessionSnapshot>,
        active_session: u64,
        windows: Vec<PresentationWindowSummary>,
    },
    SessionCommandCompleted {
        actor_id: String,
        completion: SessionCommandCompletion,
    },
    RemoteTerminalInputSent {
        backend: brood_protocol::BackendId,
        trace_sequence: u64,
        queued_at: std::time::Instant,
        sent_at: std::time::Instant,
    },
    BackendActorMessage {
        backend: brood_protocol::BackendId,
        realm: BackendActorRealm,
        sender: String,
        recipient: String,
        body: Vec<u8>,
        received_at: std::time::Instant,
    },
}

mod actor_runtime {
    pub const ACTOR_OUTPUT_BACKLOG: usize = 256;

    pub struct BackendActorMessageRequest {
        pub backend: brood_protocol::BackendId,
        pub realm: brood_protocol::BackendActorRealm,
        pub sender: String,
        pub actor: String,
        pub body: Vec<u8>,
    }

    pub struct SessionCommandEnvelope {
        pub actor_id: String,
        pub request: brood_wasm::SessionCommandRequest,
    }
}

fn terminal_input_sequence(_body: &[u8]) -> Option<u64> {
    None
}

#[derive(Clone)]
struct ClientCatalog {
    sessions: SessionSnapshot,
    windows: Vec<CatalogWireWindow>,
}

struct BackendActorMessage {
    backend: u64,
    sender: String,
    realm: BackendActorRealm,
    recipient: String,
    body: Vec<u8>,
}

struct PendingScene {
    sequence: u64,
    expected: usize,
    width: u32,
    height: u32,
    panes: Vec<(u64, u64, [i32; 4], Option<u64>)>,
}

#[derive(Debug)]
struct CanvasPresentationHost {
    presentation: PresentationHandle,
    actors: Mutex<BTreeMap<u64, CanvasActorState>>,
    next_resource: AtomicU64,
    next_revision: AtomicU64,
    resize_ready: mpsc::SyncSender<(u64, u64)>,
}

#[derive(Clone, Debug)]
struct CanvasActorState {
    pane: u64,
    requested: bool,
    subscribed: bool,
    hint_in_flight: bool,
    managed_document: bool,
    canvas_content: bool,
    canvas_revision: Option<u64>,
    dirty_revision: u64,
    retained_resource_bytes: usize,
    resources: BTreeMap<u64, usize>,
}

const MAX_CANVAS_RESOURCES_PER_ACTOR: usize = 4_096;
const MAX_CANVAS_RESOURCE_BYTES_PER_ACTOR: usize = 128 * 1024 * 1024;
const MAX_CANVAS_IMAGE_DIMENSION: u32 = 16_384;
const MAX_CANVAS_TEXT_BYTES: usize = 4 * 1024 * 1024;

impl CanvasPresentationHost {
    fn new(presentation: PresentationHandle, resize_ready: mpsc::SyncSender<(u64, u64)>) -> Self {
        Self {
            presentation,
            actors: Mutex::new(BTreeMap::new()),
            next_resource: AtomicU64::new(1),
            next_revision: AtomicU64::new(1),
            resize_ready,
        }
    }

    fn register(&self, generation: u64, pane: u64) {
        self.actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                generation,
                CanvasActorState {
                    pane,
                    requested: true,
                    subscribed: false,
                    hint_in_flight: false,
                    managed_document: false,
                    canvas_content: false,
                    canvas_revision: None,
                    dirty_revision: 1,
                    retained_resource_bytes: 0,
                    resources: BTreeMap::new(),
                },
            );
    }

    fn unregister(&self, generation: u64) {
        let removed = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&generation);
        if let Some(actor) = removed {
            for resource in actor.resources.keys().copied() {
                let (response, _receiver) = mpsc::sync_channel(1);
                drop(
                    self.presentation
                        .try_send(PresentationCommand::RemoveCanvasResource { resource, response }),
                );
            }
        }
    }

    fn mark_managed(&self, generation: u64) {
        if let Some(actor) = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&generation)
        {
            actor.managed_document = true;
        }
    }

    fn reserve_resource(
        &self,
        generation: u64,
        pane: u64,
        bytes: usize,
    ) -> Result<u64, PresentationError> {
        let mut actors = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let actor = actors
            .get_mut(&generation)
            .ok_or(PresentationError::ActorStopped)?;
        if actor.pane != pane {
            return Err(PresentationError::InvalidPane);
        }
        if actor.resources.len() >= MAX_CANVAS_RESOURCES_PER_ACTOR
            || actor
                .retained_resource_bytes
                .checked_add(bytes)
                .is_none_or(|total| total > MAX_CANVAS_RESOURCE_BYTES_PER_ACTOR)
        {
            return Err(PresentationError::LimitReached);
        }
        let resource = self.next_resource.fetch_add(1, Ordering::Relaxed);
        if resource == 0 || resource == u64::MAX {
            return Err(PresentationError::LimitReached);
        }
        actor.retained_resource_bytes += bytes;
        actor.resources.insert(resource, bytes);
        Ok(resource)
    }

    fn forget_resource(&self, generation: u64, resource: u64) {
        let mut actors = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(actor) = actors.get_mut(&generation)
            && let Some(bytes) = actor.resources.remove(&resource)
        {
            actor.retained_resource_bytes = actor.retained_resource_bytes.saturating_sub(bytes);
        }
    }

    fn render_targets(&self) -> Vec<(u64, u64, u64)> {
        self.actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .filter_map(|(generation, state)| {
                // Keep at most one subscription opportunity in flight. This gives continuous
                // actors a vsync-paced callback without allowing render hints to starve input.
                if !state.hint_in_flight && (state.requested || state.subscribed) {
                    state.requested = false;
                    state.hint_in_flight = true;
                    Some((*generation, state.pane, state.dirty_revision))
                } else {
                    None
                }
            })
            .collect()
    }

    fn canvas_revision(&self, generation: u64) -> Option<u64> {
        self.actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&generation)
            .and_then(|actor| actor.canvas_revision)
    }

    fn is_managed_only(&self, generation: u64) -> bool {
        self.actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&generation)
            .is_some_and(|actor| actor.managed_document && !actor.canvas_content)
    }

    fn accepts_raw_pointer(&self, pane: u64) -> bool {
        self.actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|actor| actor.pane == pane)
            .is_none_or(|actor| !actor.managed_document || actor.canvas_content)
    }

    fn accepts_raw_keyboard(&self, pane: u64) -> bool {
        self.accepts_raw_pointer(pane)
    }
}

impl PresentationHost for CanvasPresentationHost {
    fn request_render(&self, actor_generation: u64) -> Result<(), PresentationError> {
        let mut actors = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let actor = actors
            .get_mut(&actor_generation)
            .ok_or(PresentationError::ActorStopped)?;
        actor.requested = true;
        actor.dirty_revision = actor.dirty_revision.saturating_add(1).max(1);
        Ok(())
    }

    fn set_render_subscription(
        &self,
        actor_generation: u64,
        enabled: bool,
    ) -> Result<(), PresentationError> {
        let mut actors = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let actor = actors
            .get_mut(&actor_generation)
            .ok_or(PresentationError::ActorStopped)?;
        actor.subscribed = enabled;
        Ok(())
    }

    fn finish_actor_frame(
        &self,
        actor_generation: u64,
        _token: RenderToken,
        draws: Vec<ActorFrameDraw>,
    ) -> Result<u64, PresentationError> {
        if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some() {
            eprintln!(
                "presentation actor frame generation={actor_generation} draws={}",
                draws.len()
            );
        }
        let (owned, managed_document, resources) = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&actor_generation)
            .map(|actor| {
                (
                    actor.pane,
                    actor.managed_document,
                    actor.resources.keys().copied().collect::<BTreeSet<_>>(),
                )
            })
            .ok_or(PresentationError::ActorStopped)?;
        let prepared = (|| {
            if draws.len() > 1 {
                return Err(PresentationError::DuplicatePane);
            }
            draws
                .into_iter()
                .map(|draw| {
                    if draw.pane != owned {
                        return Err(PresentationError::InvalidPane);
                    }
                    let packet = brood_graphics::Packet::decode(&draw.packet)
                        .map_err(|_| PresentationError::InvalidPacket)?;
                    if !crate::canvas_packet_is_exact(&packet) {
                        return Err(PresentationError::InvalidPacket);
                    }
                    if packet.commands.iter().any(|command| match command {
                        brood_graphics::Command::DrawImage { image, .. } => {
                            !resources.contains(image)
                        }
                        brood_graphics::Command::DrawTextLayout { layout, .. } => {
                            !resources.contains(layout)
                        }
                        _ => false,
                    }) {
                        return Err(PresentationError::InvalidResource);
                    }
                    Ok((draw, packet))
                })
                .collect::<Result<Vec<_>, _>>()
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(actor) = self
                    .actors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&actor_generation)
                {
                    actor.hint_in_flight = false;
                }
                return Err(error);
            }
        };
        if let Some(actor) = self
            .actors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&actor_generation)
        {
            actor.hint_in_flight = false;
            actor.canvas_content |= !prepared.is_empty();
        }
        let revision = self.next_revision.fetch_add(1, Ordering::Relaxed);
        let changed = !prepared.is_empty();
        for (draw, packet) in prepared {
            let document = brood_protocol::managed_ui::UiDocument {
                protocol_version: brood_protocol::managed_ui::MANAGED_UI_PROTOCOL_VERSION,
                pane: draw.pane,
                document_revision: revision,
                root: brood_protocol::managed_ui::UiNode {
                    id: 1,
                    kind: brood_protocol::managed_ui::UiNodeKind::Canvas { target: draw.pane },
                    style: brood_protocol::managed_ui::UiStyle {
                        grow: 1.0,
                        ..Default::default()
                    },
                    accessibility: brood_protocol::managed_ui::UiAccessibility {
                        label: Some("Plugin canvas".to_string()),
                        ..Default::default()
                    },
                    optional: false,
                    interaction_revision: 0,
                    children: Vec::new(),
                },
            };
            let document = (!managed_document).then_some(Box::new(document));
            if let Some(epoch) = draw.resize_epoch {
                self.presentation
                    .try_send(PresentationCommand::StageCanvas {
                        actor_generation,
                        document,
                        epoch,
                        target: draw.pane,
                        packet: Arc::new(packet),
                    })
                    .map_err(|_| PresentationError::LimitReached)?;
                let _ = self.resize_ready.try_send((draw.pane, epoch));
            } else {
                self.presentation
                    .try_send(PresentationCommand::ReplaceCanvas {
                        actor_generation,
                        document,
                        target: draw.pane,
                        packet: Arc::new(packet),
                    })
                    .map_err(|_| PresentationError::LimitReached)?;
            }
        }
        if changed
            && let Some(actor) = self
                .actors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&actor_generation)
        {
            actor.canvas_revision = Some(revision);
        }
        Ok(revision)
    }

    fn reuse_pane_content(
        &self,
        _actor_generation: u64,
        pane: u64,
        resize_epoch: u64,
    ) -> Result<(), PresentationError> {
        self.resize_ready
            .try_send((pane, resize_epoch))
            .map_err(|_| PresentationError::LimitReached)
    }

    fn create_image_rgba8(
        &self,
        actor_generation: u64,
        pane: u64,
        width: u32,
        height: u32,
        mut pixels: Vec<u8>,
    ) -> Result<u64, PresentationError> {
        if width == 0
            || height == 0
            || width > MAX_CANVAS_IMAGE_DIMENSION
            || height > MAX_CANVAS_IMAGE_DIMENSION
        {
            return Err(PresentationError::InvalidResource);
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height)?.checked_mul(4))
            })
            .ok_or(PresentationError::InvalidResource)?;
        if pixels.len() != expected {
            return Err(PresentationError::InvalidResource);
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(width, height, pixels)
            .ok_or(PresentationError::InvalidResource)?;
        let resource = self.reserve_resource(actor_generation, pane, expected)?;
        let image = Arc::new(gpui::RenderImage::new(vec![image::Frame::new(buffer)]));
        if self
            .presentation
            .try_send(PresentationCommand::ReplaceCanvasImage { resource, image })
            .is_err()
        {
            self.forget_resource(actor_generation, resource);
            return Err(PresentationError::LimitReached);
        }
        Ok(resource)
    }

    fn layout_text(
        &self,
        actor_generation: u64,
        pane: u64,
        text: String,
        options: TextLayoutOptions,
    ) -> Result<TextLayoutResult, PresentationError> {
        if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some() {
            eprintln!(
                "presentation actor text layout request bytes={} size={}",
                text.len(),
                options.font_size
            );
        }
        if text.len() > MAX_CANVAS_TEXT_BYTES
            || !options.font_size.is_finite()
            || !options.max_width.is_finite()
            || !options.line_height.is_finite()
            || options.font_size <= 0.0
            || options.max_width <= 0.0
            || options.line_height <= 0.0
            || !options
                .color
                .into_iter()
                .all(|channel| channel.is_finite() && (0.0..=1.0).contains(&channel))
        {
            return Err(PresentationError::InvalidArgument);
        }
        // Charge a conservative glyph/cache allowance in addition to UTF-8
        // storage so adversarial unique text cannot evade the retained-byte cap.
        let retained_bytes = text.len().saturating_mul(64).saturating_add(4_096);
        let resource = self.reserve_resource(actor_generation, pane, retained_bytes)?;
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        if self
            .presentation
            .try_send(PresentationCommand::MeasureCanvasText {
                resource,
                layout: crate::CanvasTextLayout { text, options },
                response: response_sender,
            })
            .is_err()
        {
            self.forget_resource(actor_generation, resource);
            return Err(PresentationError::LimitReached);
        }
        let result = response_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| PresentationError::HostFailure)?;
        if result.is_err() {
            self.forget_resource(actor_generation, resource);
        }
        result
    }

    fn drop_resource(
        &self,
        actor_generation: u64,
        pane: u64,
        resource: u64,
    ) -> Result<(), PresentationError> {
        {
            let actors = self
                .actors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let actor = actors
                .get(&actor_generation)
                .ok_or(PresentationError::ActorStopped)?;
            if actor.pane != pane {
                return Err(PresentationError::InvalidPane);
            }
            if !actor.resources.contains_key(&resource) {
                return Err(PresentationError::InvalidResource);
            }
        }
        let (response, receiver) = mpsc::sync_channel(1);
        self.presentation
            .try_send(PresentationCommand::RemoveCanvasResource { resource, response })
            .map_err(|_| PresentationError::LimitReached)?;
        if !receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| PresentationError::HostFailure)?
        {
            return Err(PresentationError::ResourceInUse);
        }
        self.forget_resource(actor_generation, resource);
        Ok(())
    }
}

pub fn run() -> Result<()> {
    let args = ClientArgs::parse();
    let (presentation, commands) = presentation_channel(256);
    let (action_sender, action_receiver) = mpsc::sync_channel(1_024);
    let actions = ActionSink::new(action_sender);
    let (completion_sender, completion_receiver) = mpsc::sync_channel(64);
    let (local_completion_sender, local_completion_receiver) = mpsc::sync_channel(64);
    let (input_sender, native_input_receiver) =
        mpsc::sync_channel::<brood_protocol::InputEvent>(1_024);
    let (server_input_sender, transport_input_receiver) = mpsc::sync_channel(1_024);
    let supervisor_input_sender = server_input_sender.clone();
    let (actor_input_sender, actor_input_receiver) = mpsc::sync_channel(1_024);
    // A virtual keyboard can enqueue key-down, committed text, and key-up at several hundred Hz
    // while a canvas actor is finishing a frame. Keep this bounded but large enough to preserve
    // the reliable input lane through that transient backpressure.
    let (actor_direct_sender, actor_direct_receiver) = mpsc::sync_channel(65_536);
    thread::Builder::new()
        .name("brood-gpui-input-router".to_string())
        .spawn(move || {
            while let Ok(event) = native_input_receiver.recv() {
                drop(actor_input_sender.try_send(event.clone()));
                drop(server_input_sender.try_send(event));
            }
        })
        .context("spawn GPUI input router")?;
    let (catalog_sender, catalog_receiver) = mpsc::sync_channel(16);
    let (backend_actor_sender, backend_actor_receiver) = mpsc::sync_channel(128);
    let (presentation_actor_sender, presentation_actor_receiver) = mpsc::sync_channel(128);
    let (remote_session_sender, remote_session_receiver) = mpsc::sync_channel(128);
    let (remote_artifact_sender, remote_artifact_receiver) = mpsc::sync_channel(32);
    let (resize_ready_sender, resize_ready_receiver) = mpsc::sync_channel(256);
    let (clipboard_sender, clipboard_receiver) = mpsc::sync_channel(16);
    let attach_local = !args.no_server && (args.remote_connect.is_empty() || args.include_local);
    let control_target = attach_local
        .then(|| resolve_control_target(args.dbus_name.clone(), args.dbus_address.clone()));

    let (remote_event_sender, remote_event_receiver) = mpsc::sync_channel(256);
    let remote_inputs = Arc::new(Mutex::new(BTreeMap::<u64, remote::RemoteInputLane>::new()));
    let remote_actors = Arc::new(Mutex::new(
        BTreeMap::<u64, remote::RemoteActorMessageLane>::new(),
    ));
    let remote_commands = Arc::new(Mutex::new(
        BTreeMap::<u64, remote::RemoteBackendCommandLane>::new(),
    ));
    let remote_frame_resources =
        Arc::new(Mutex::new(BTreeMap::<u64, (u64, u64, BTreeSet<u64>)>::new()));
    let mut offers = args
        .remote_offer_session_artifacts
        .iter()
        .map(|spec| {
            brood_transport::ArtifactOffer::parse(spec, brood_protocol::ArtifactRealm::Session)
        })
        .collect::<Result<Vec<_>>>()?;
    for spec in &args.remote_offer_session_packages {
        offers.extend(brood_transport::package_offers(
            spec,
            brood_protocol::ArtifactRealm::Session,
        )?);
    }
    let mut remote_connections = Vec::new();
    if !args.remote_connect.is_empty() {
        for (index, connect) in args.remote_connect.iter().copied().enumerate() {
            let backend = brood_protocol::BackendId(
                u64::try_from(index).unwrap_or(u64::MAX).saturating_add(2),
            );
            let mut connection = remote::RemoteConnection::start(
                remote::RemoteConfig {
                    backend,
                    connect,
                    server_name: args.remote_server_name.clone(),
                    server_certificate: args
                        .remote_server_certificate
                        .clone()
                        .context("remote server certificate is missing")?,
                    client_certificate: args
                        .remote_client_certificate
                        .clone()
                        .context("remote client certificate is missing")?,
                    client_private_key: args
                        .remote_client_private_key
                        .clone()
                        .context("remote client private key is missing")?,
                    artifact_offers: offers.clone(),
                    allowed_client_artifacts: args.remote_allow_client_artifacts.clone(),
                    artifact_cache: args
                        .remote_artifact_cache
                        .clone()
                        .unwrap_or_else(brood_transport::default_artifact_cache),
                },
                &EventProxy(remote_event_sender.clone()),
            )?;
            let (input, actors, commands) = connection.take_lanes()?;
            remote_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(backend.0, input);
            remote_actors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(backend.0, actors);
            remote_commands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(backend.0, commands);
            connection.activate_disconnect_notifications()?;
            eprintln!(
                "selected GPUI remote backend={} generation={} peer={connect}",
                backend.0, connection.generation
            );
            remote_connections.push(connection);
        }
    }
    let dynamic_remote_template = match (
        args.remote_server_certificate.clone(),
        args.remote_client_certificate.clone(),
        args.remote_client_private_key.clone(),
    ) {
        (Some(server_certificate), Some(client_certificate), Some(client_private_key)) => {
            Some(remote::RemoteConfig {
                backend: brood_protocol::BackendId(0),
                connect: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
                server_name: args.remote_server_name.clone(),
                server_certificate,
                client_certificate,
                client_private_key,
                artifact_offers: offers,
                allowed_client_artifacts: args.remote_allow_client_artifacts.clone(),
                artifact_cache: args
                    .remote_artifact_cache
                    .clone()
                    .unwrap_or_else(brood_transport::default_artifact_cache),
            })
        }
        _ => None,
    };
    let dynamic_backends = spawn_dynamic_backend_manager(
        dynamic_remote_template,
        u64::try_from(remote_connections.len())
            .unwrap_or(u64::MAX)
            .saturating_add(2),
        remote_event_sender.clone(),
        Arc::clone(&remote_inputs),
        Arc::clone(&remote_actors),
        Arc::clone(&remote_commands),
        remote_connections,
    )?;

    let (local_input_sender, local_input_receiver) = mpsc::sync_channel(1_024);
    spawn_transport_input_router(
        transport_input_receiver,
        attach_local.then_some(local_input_sender),
        Arc::clone(&remote_inputs),
    )?;
    let (local_actor_sender, local_actor_receiver) = mpsc::sync_channel(128);
    spawn_backend_actor_router(
        backend_actor_receiver,
        attach_local.then_some(local_actor_sender),
        Arc::clone(&remote_actors),
    )?;
    spawn_remote_event_router(
        remote_event_receiver,
        presentation.clone(),
        catalog_sender.clone(),
        presentation_actor_sender.clone(),
        Arc::clone(&remote_inputs),
        Arc::clone(&remote_actors),
        Arc::clone(&remote_commands),
        resize_ready_sender.clone(),
        remote_session_sender,
        remote_artifact_sender,
        Arc::clone(&remote_frame_resources),
    )?;

    spawn_frame_completion_router(
        completion_receiver,
        attach_local.then_some(local_completion_sender),
        Arc::clone(&remote_inputs),
        remote_frame_resources,
    )?;

    let bound_session = if let Some(target) = control_target.as_ref() {
        Some(start_local_transport(
            &args,
            target,
            presentation.clone(),
            local_completion_receiver,
            local_input_receiver,
            catalog_sender,
            local_actor_receiver,
            presentation_actor_sender,
            resize_ready_sender.clone(),
        )?)
    } else {
        None
    };
    start_actor_supervisor(
        &args,
        presentation.clone(),
        action_receiver,
        catalog_receiver,
        control_target,
        bound_session,
        actor_input_receiver,
        backend_actor_sender,
        presentation_actor_receiver,
        remote_commands,
        remote_session_receiver,
        remote_artifact_receiver,
        dynamic_backends,
        resize_ready_sender,
        resize_ready_receiver,
        actor_direct_receiver,
        supervisor_input_sender,
        clipboard_sender,
        clipboard_receiver,
    )?;

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let mut root = None;
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Brood".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                let entity = cx.new(|cx| {
                    let mut view = ManagedUiRoot::new(Vec::new(), actions.clone(), cx);
                    view.set_frame_completion_sink(completion_sender);
                    view.set_input_sink(input_sender);
                    view.set_actor_input_sink(actor_direct_sender);
                    view
                });
                root = Some(entity.clone());
                entity
            },
        );
        if let Err(error) = window {
            eprintln!("could not open GPUI presentation: {error}");
            cx.quit();
            return;
        }
        let Some(root) = root else {
            eprintln!("GPUI did not construct the presentation root");
            cx.quit();
            return;
        };
        cx.spawn(async move |cx| {
            drive_presentation(commands, root, actions, cx.clone()).await;
        })
        .detach();
        cx.activate(true);
    });
    Ok(())
}

const REMOTE_PANE_BASE: u64 = 1_u64 << 62;
const LOCAL_PLUGIN_PANE_BASE: u64 = 1_u64 << 63;

fn remote_pane(backend: u64, pane: u64) -> u64 {
    REMOTE_PANE_BASE | ((backend & 0x3fff) << 48) | (pane & 0x0000_ffff_ffff_ffff)
}

fn remote_pane_route(pane: u64) -> Option<(u64, u64)> {
    (pane & (3_u64 << 62) == REMOTE_PANE_BASE)
        .then_some(((pane >> 48) & 0x3fff, pane & 0x0000_ffff_ffff_ffff))
}

fn spawn_transport_input_router(
    input: mpsc::Receiver<brood_protocol::InputEvent>,
    local: Option<mpsc::SyncSender<brood_protocol::InputEvent>>,
    remotes: Arc<Mutex<BTreeMap<u64, remote::RemoteInputLane>>>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-transport-input".into())
        .spawn(move || {
            let mut focused_remote = None;
            while let Ok(mut event) = input.recv() {
                let pane = match &event {
                    brood_protocol::InputEvent::WindowFocus { pane }
                    | brood_protocol::InputEvent::WindowResize { pane, .. } => Some(*pane),
                    brood_protocol::InputEvent::PointerMotion { position }
                    | brood_protocol::InputEvent::PointerButton { position, .. }
                    | brood_protocol::InputEvent::PointerAxis { position, .. }
                    | brood_protocol::InputEvent::PointerWheel { position, .. } => {
                        Some(position.pane)
                    }
                    _ => None,
                };
                if pane.is_some_and(|pane| pane >= LOCAL_PLUGIN_PANE_BASE) {
                    continue;
                }
                if let Some((backend, backend_pane)) = pane.and_then(remote_pane_route) {
                    translate_input_pane(&mut event, backend_pane);
                    focused_remote = Some(backend);
                    if let Some(lane) = remotes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&backend)
                    {
                        let _ = lane.send(ClientMessage::Input(event));
                    }
                    continue;
                }
                let broadcast = matches!(
                    event,
                    brood_protocol::InputEvent::PresentationSize { .. }
                        | brood_protocol::InputEvent::PresentationFocus { .. }
                        | brood_protocol::InputEvent::KeyboardReset
                        | brood_protocol::InputEvent::PointerLeave
                );
                if broadcast {
                    if let Some(local) = local.as_ref() {
                        let _ = local.try_send(event.clone());
                    }
                    for lane in remotes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .values_mut()
                    {
                        let _ = lane.send(ClientMessage::Input(event.clone()));
                    }
                } else if let Some(backend) = focused_remote
                    && matches!(event, brood_protocol::InputEvent::KeyboardKey { .. })
                {
                    if let Some(lane) = remotes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get_mut(&backend)
                    {
                        let _ = lane.send(ClientMessage::Input(event));
                    }
                } else if let Some(local) = local.as_ref() {
                    let _ = local.try_send(event);
                }
            }
        })
        .context("spawn GPUI transport input router")?;
    Ok(())
}

fn translate_input_pane(event: &mut brood_protocol::InputEvent, pane: u64) {
    match event {
        brood_protocol::InputEvent::WindowFocus { pane: target }
        | brood_protocol::InputEvent::WindowResize { pane: target, .. } => *target = pane,
        brood_protocol::InputEvent::PointerMotion { position }
        | brood_protocol::InputEvent::PointerButton { position, .. }
        | brood_protocol::InputEvent::PointerAxis { position, .. }
        | brood_protocol::InputEvent::PointerWheel { position, .. } => position.pane = pane,
        _ => {}
    }
}

fn spawn_backend_actor_router(
    messages: mpsc::Receiver<BackendActorMessage>,
    local: Option<mpsc::SyncSender<BackendActorMessage>>,
    remotes: Arc<Mutex<BTreeMap<u64, remote::RemoteActorMessageLane>>>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-actor-router".into())
        .spawn(move || {
            while let Ok(message) = messages.recv() {
                if message.backend == 1 {
                    if let Some(local) = local.as_ref() {
                        let _ = local.try_send(message);
                    }
                } else if let Some(lane) = remotes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&message.backend)
                {
                    let request = actor_runtime::BackendActorMessageRequest {
                        backend: brood_protocol::BackendId(message.backend),
                        realm: message.realm,
                        sender: message.sender,
                        actor: message.recipient,
                        body: message.body,
                    };
                    if let Err(error) = lane.send(&request) {
                        eprintln!(
                            "remote actor message failed backend={}: {error:#}",
                            message.backend
                        );
                    }
                }
            }
        })
        .context("spawn GPUI backend actor router")?;
    Ok(())
}

fn spawn_dynamic_backend_manager(
    template: Option<remote::RemoteConfig>,
    first_backend: u64,
    events: mpsc::SyncSender<ClientEvent>,
    inputs: Arc<Mutex<BTreeMap<u64, remote::RemoteInputLane>>>,
    actors: Arc<Mutex<BTreeMap<u64, remote::RemoteActorMessageLane>>>,
    commands: Arc<Mutex<BTreeMap<u64, remote::RemoteBackendCommandLane>>>,
    mut connections: Vec<remote::RemoteConnection>,
) -> Result<DynamicBackendHandle> {
    let deployer = backend_deployer_path();
    let can_connect_remote = template.is_some();
    let (sender, receiver) = mpsc::sync_channel::<DynamicBackendRequest>(16);
    thread::Builder::new()
        .name("brood-gpui-backend-manager".into())
        .spawn(move || {
            let mut next_backend = first_backend;
            let mut deployments = Vec::<BackendDeployment>::new();
            while let Ok(request) = receiver.recv() {
                let result = (|| -> Result<()> {
                    let (mut config, deployment) = match request {
                        DynamicBackendRequest::Connect(address) => {
                            let connect = address
                                .to_socket_addrs()
                                .with_context(|| {
                                    format!("resolve remote backend address {address:?}")
                                })?
                                .next()
                                .context("remote backend address resolved to no endpoint")?;
                            let mut config =
                                template.clone().context("remote credentials unavailable")?;
                            config.connect = connect;
                            (config, None)
                        }
                        DynamicBackendRequest::Attach { config, deployment } => {
                            (*config, Some(deployment))
                        }
                    };
                    let backend = brood_protocol::BackendId(next_backend);
                    next_backend = next_backend.saturating_add(1);
                    anyhow::ensure!(
                        backend.0 < 0x4000,
                        "presentation backend identity space exhausted"
                    );
                    config.backend = backend;
                    let connect = config.connect;
                    let mut connection =
                        remote::RemoteConnection::start(config, &EventProxy(events.clone()))?;
                    let (input, actor_lane, command_lane) = connection.take_lanes()?;
                    inputs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(backend.0, input);
                    actors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(backend.0, actor_lane);
                    commands
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(backend.0, command_lane);
                    connection.activate_disconnect_notifications()?;
                    eprintln!(
                        "connected dynamic GPUI remote backend={} peer={connect}",
                        backend.0
                    );
                    connections.push(connection);
                    if let Some(deployment) = deployment {
                        deployments.push(deployment);
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("could not connect dynamic remote backend: {error:#}");
                }
            }
            for connection in &mut connections {
                let _ = connection.shutdown();
            }
            for deployment in &mut deployments {
                deployment.shutdown();
            }
        })
        .context("spawn dynamic backend manager")?;
    Ok(DynamicBackendHandle {
        sender,
        can_connect_remote,
        deployer,
    })
}

impl BackendDeployment {
    fn shutdown(&mut self) {
        drop(self.stdin.take());
        if let Err(error) = self.child.wait() {
            eprintln!(
                "could not reap backend deployment {}: {error}",
                self.description
            );
        }
    }
}

impl Drop for BackendDeployment {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn backend_deployer_path() -> Option<PathBuf> {
    std::env::var_os("BROOD_REMOTE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|executable| executable.with_file_name("brood-remote"))
                .filter(|deployer| deployer.is_file())
        })
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join("brood-remote"))
                    .find(|deployer| deployer.is_file())
            })
        })
        .filter(|deployer| deployer.is_file())
}

fn spawn_backend_deployment(
    deployer: PathBuf,
    kind: DeploymentKind,
    sender: mpsc::SyncSender<DynamicBackendRequest>,
) -> Result<()> {
    let client = std::env::current_exe().context("resolve running brood-client executable")?;
    let (description, mut command) = match &kind {
        DeploymentKind::Ssh { target } => {
            let mut command = Command::new(&deployer);
            command
                .arg("attach-ssh")
                .arg(target)
                .arg("--client")
                .arg(&client);
            if let Some(connect_host) =
                std::env::var_os("BROOD_SSH_CONNECT_HOST").filter(|host| !host.is_empty())
            {
                command.arg("--connect-host").arg(connect_host);
            }
            (format!("SSH backend {target}"), command)
        }
        DeploymentKind::Container { image, connection } => {
            let mut command = Command::new(&deployer);
            command
                .arg("attach-container")
                .arg(image)
                .arg("--client")
                .arg(&client);
            if let Some(connection) = connection {
                command.arg("--connection").arg(connection);
            }
            (
                connection.as_ref().map_or_else(
                    || format!("container backend {image}"),
                    |connection| format!("container backend {connection}::{image}"),
                ),
                command,
            )
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {description}"))?;
    let stdin = child
        .stdin
        .take()
        .with_context(|| format!("capture {description} stdin"))?;
    let stdout = child
        .stdout
        .take()
        .with_context(|| format!("capture {description} stdout"))?;
    thread::Builder::new()
        .name("brood-gpui-backend-deploy".into())
        .spawn(move || {
            let outcome = (|| -> Result<(remote::RemoteConfig, BackendDeployment)> {
                let mut line = String::new();
                let bytes = BufReader::new(stdout)
                    .read_line(&mut line)
                    .with_context(|| format!("read {description} attachment details"))?;
                anyhow::ensure!(
                    bytes != 0,
                    "{description} exited before publishing attachment details"
                );
                let ready: BackendAttachReady = serde_json::from_str(line.trim_end())
                    .with_context(|| format!("decode {description} attachment details"))?;
                let offer = format!("{}={}", ready.session_package, ready.session_root.display());
                let config = remote::RemoteConfig {
                    backend: brood_protocol::BackendId(0),
                    connect: ready.connect,
                    server_name: ready.server_name,
                    server_certificate: ready.server_certificate,
                    client_certificate: ready.client_certificate,
                    client_private_key: ready.client_private_key,
                    artifact_offers: brood_transport::package_offers(
                        &offer,
                        brood_protocol::ArtifactRealm::Session,
                    )?,
                    allowed_client_artifacts: Vec::new(),
                    artifact_cache: brood_transport::default_artifact_cache(),
                };
                Ok((
                    config,
                    BackendDeployment {
                        description,
                        child,
                        stdin: Some(stdin),
                    },
                ))
            })();
            match outcome {
                Ok((config, deployment)) => {
                    if let Err(error) = sender.send(DynamicBackendRequest::Attach {
                        config: Box::new(config),
                        deployment,
                    }) {
                        let DynamicBackendRequest::Attach { mut deployment, .. } = error.0 else {
                            return;
                        };
                        deployment.shutdown();
                    }
                }
                Err(error) => eprintln!("backend deployment failed: {error:#}"),
            }
        })
        .context("spawn backend deployment reader")?;
    Ok(())
}

fn spawn_remote_event_router(
    events: mpsc::Receiver<ClientEvent>,
    presentation: PresentationHandle,
    catalogs: mpsc::SyncSender<ClientCatalog>,
    actor_messages: mpsc::SyncSender<BackendActorMessage>,
    remote_inputs: Arc<Mutex<BTreeMap<u64, remote::RemoteInputLane>>>,
    remote_actors: Arc<Mutex<BTreeMap<u64, remote::RemoteActorMessageLane>>>,
    remote_commands: Arc<Mutex<BTreeMap<u64, remote::RemoteBackendCommandLane>>>,
    resize_ready: mpsc::SyncSender<(u64, u64)>,
    session_completions: mpsc::SyncSender<(String, SessionCommandCompletion)>,
    artifact_installs: mpsc::SyncSender<(
        brood_protocol::ArtifactManifest,
        std::path::PathBuf,
        tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    )>,
    frame_resources: Arc<Mutex<BTreeMap<u64, (u64, u64, BTreeSet<u64>)>>>,
) -> Result<()> {
    thread::Builder::new().name("brood-gpui-remote-events".into()).spawn(move || {
        while let Ok(event) = events.recv() {
            match event {
                ClientEvent::SceneFrame { backend, connection_generation, sequence, width, height, windows } => {
                    let count = windows.len().max(1) as f32;
                    let pane_width = width as f32 / count;
                    let mut panes = Vec::new();
                    let mut resources = BTreeSet::new();
                    let mut ready = Vec::new();
                    for (index, window) in windows.into_iter().enumerate() {
                        let _ = (window.backend, window.backend_window_id, window.window_id, window.active, window.resize_epoch);
                        let Some(FrameResource::Network(frame)) = window.resource else { continue };
                        match network_frame_image(&frame, window.rect) {
                            Ok(image) => {
                                resources.insert(window.resource_id);
                                if let Some(commands) = remote_commands
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .get(&backend.0)
                                {
                                    let _ = commands.try_release_resource(window.resource_id);
                                }
                                panes.push(CopiedPane {
                                pane: remote_pane(backend.0, window.window_id),
                                resource: remote_pane(backend.0, window.resource_id),
                                geometry: PaneGeometry { x: index as f32 * pane_width, y: 0.0, width: pane_width, height: height as f32 },
                                image,
                            });
                                if let Some(epoch) = window.resize_epoch {
                                    ready.push((remote_pane(backend.0, window.window_id), epoch));
                                }
                            },
                            Err(error) => eprintln!("rejected remote frame backend={} generation={connection_generation}: {error:#}", backend.0),
                        }
                    }
                    let revision = remote_pane(backend.0, sequence);
                    let mut tracked = frame_resources.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    tracked.retain(|_, (candidate_backend, _, _)| *candidate_backend != backend.0);
                    tracked.insert(revision, (backend.0, sequence, resources));
                    let resize_epoch = ready.first().map(|(_, epoch)| *epoch)
                        .filter(|epoch| ready.iter().all(|(_, candidate)| candidate == epoch));
                    let command = match resize_epoch {
                        Some(epoch) => PresentationCommand::StageCopiedScene { epoch, sequence: revision, panes },
                        None => PresentationCommand::CommitCopiedScene { revision, sequence: revision, panes },
                    };
                    if presentation.try_send(command).is_ok() {
                        for readiness in ready { let _ = resize_ready.try_send(readiness); }
                    }
                }
                ClientEvent::BackendSnapshot { backend, session, active_session, windows } => {
                    let routes = windows.iter().map(|window| (remote_pane(backend.0, window.pane), window.pane)).collect::<Vec<_>>();
                    if let Some(lane) = remote_inputs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&backend.0) {
                        lane.set_pane_routes(routes);
                    }
                    if let Some(mut sessions) = session {
                        sessions.backend_id = backend.0;
                        let windows = windows.into_iter().map(|window| CatalogWireWindow {
                            pane: remote_pane(backend.0, window.pane),
                            session: window.session,
                            app_id: window.app_id,
                            _size_authority: window.size_authority,
                            _automation: window.automation,
                        }).collect();
                        let _ = catalogs.try_send(ClientCatalog { sessions, windows });
                    } else {
                        eprintln!("remote backend={} has no session snapshot active={active_session}", backend.0);
                    }
                }
                ClientEvent::BackendActorMessage { backend, realm, sender, recipient, body, received_at } => {
                    let _ = received_at;
                    let _ = actor_messages.try_send(BackendActorMessage { backend: backend.0, sender, realm, recipient, body });
                }
                ClientEvent::ClosePresentation => { let _ = presentation.try_send(PresentationCommand::Shutdown); }
                ClientEvent::FocusPresentation => {}
                ClientEvent::BackendDisconnected { backend, message } => {
                    remote_inputs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&backend.0);
                    remote_actors.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&backend.0);
                    remote_commands.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&backend.0);
                    frame_resources.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                        .retain(|_, (candidate, _, _)| *candidate != backend.0);
                    let _ = catalogs.try_send(ClientCatalog {
                        sessions: SessionSnapshot { backend_id: backend.0, revision: u64::MAX, active: 0, sessions: Vec::new() },
                        windows: Vec::new(),
                    });
                    let _ = presentation.try_send(PresentationCommand::RemoveSceneDomain { domain: backend.0 });
                    eprintln!("remote backend={} disconnected: {message}", backend.0);
                },
                ClientEvent::ManagedResizeStatus { backend, connection_generation, status } => eprintln!("remote managed resize backend={} generation={} status={status:?}", backend.0, connection_generation),
                ClientEvent::RemoteArtifactInstalled { manifest, path, completion } => {
                    if let Err(error) = artifact_installs.try_send((manifest, path, completion)) {
                        let (manifest, _, completion) = match error {
                            mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value) => value,
                        };
                        let _ = completion.send(Err(format!("client artifact {} could not enter the bounded runtime install queue", manifest.id)));
                    }
                }
                ClientEvent::BackendCommandCompleted { description, result } => eprintln!("remote command {description}: {result:?}"),
                ClientEvent::SessionCommandCompleted { actor_id, completion } => {
                    let _ = session_completions.try_send((actor_id, completion));
                }
                ClientEvent::RemoteTerminalInputSent { backend, trace_sequence, queued_at, sent_at } => {
                    eprintln!("remote terminal input backend={} sequence={} queue_ns={}", backend.0, trace_sequence, sent_at.duration_since(queued_at).as_nanos());
                }
            }
        }
    }).context("spawn GPUI remote event router")?;
    Ok(())
}

fn spawn_frame_completion_router(
    completions: mpsc::Receiver<u64>,
    local: Option<mpsc::SyncSender<u64>>,
    remote_inputs: Arc<Mutex<BTreeMap<u64, remote::RemoteInputLane>>>,
    remote_frames: Arc<Mutex<BTreeMap<u64, (u64, u64, BTreeSet<u64>)>>>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-presented-router".into())
        .spawn(move || {
            while let Ok(encoded_sequence) = completions.recv() {
                let Some((backend, sequence, active)) = remote_frames
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&encoded_sequence)
                else {
                    if remote_pane_route(encoded_sequence).is_none()
                        && let Some(local) = local.as_ref()
                    {
                        let _ = local.try_send(encoded_sequence);
                    }
                    continue;
                };
                let _ = active;
                if let Some(input) = remote_inputs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&backend)
                {
                    let _ = input.send(ClientMessage::Lifecycle(ClientLifecycle::FramePresented {
                        sequence,
                    }));
                }
            }
        })
        .context("spawn GPUI frame-completion router")?;
    Ok(())
}

fn network_frame_image(
    frame: &remote::NetworkFrame,
    source: [i32; 4],
) -> Result<Arc<gpui::RenderImage>> {
    let x = usize::try_from(source[0]).context("remote source x")?;
    let y = usize::try_from(source[1]).context("remote source y")?;
    let width = usize::try_from(source[2]).context("remote source width")?;
    let height = usize::try_from(source[3]).context("remote source height")?;
    anyhow::ensure!(width > 0 && height > 0, "remote source is empty");
    let stride = usize::try_from(frame.stride).context("remote stride")?;
    anyhow::ensure!(
        x.saturating_add(width) <= frame.width as usize
            && y.saturating_add(height) <= frame.height as usize,
        "remote source is outside its frame"
    );
    let mut pixels = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for row in y..y + height {
        let start = row
            .saturating_mul(stride)
            .saturating_add(x.saturating_mul(4));
        let bytes = frame
            .pixels
            .get(start..start.saturating_add(width.saturating_mul(4)))
            .context("remote frame row is truncated")?;
        pixels.extend_from_slice(bytes);
    }
    if matches!(frame.pixel_format, FramePixelFormat::Bgra8) {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
        .context("construct remote image")?;
    Ok(Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
        buffer,
    )])))
}

fn start_actor_supervisor(
    args: &ClientArgs,
    presentation: PresentationHandle,
    actions: mpsc::Receiver<brood_protocol::managed_ui::UiAction>,
    catalogs: mpsc::Receiver<ClientCatalog>,
    control_target: Option<brood_cli::control::ControlTarget>,
    bound_session: Option<(u64, u64)>,
    actor_inputs: mpsc::Receiver<brood_protocol::InputEvent>,
    backend_actor_messages: mpsc::SyncSender<BackendActorMessage>,
    presentation_actor_messages: mpsc::Receiver<BackendActorMessage>,
    remote_commands: Arc<Mutex<BTreeMap<u64, remote::RemoteBackendCommandLane>>>,
    remote_session_completions: mpsc::Receiver<(String, SessionCommandCompletion)>,
    remote_artifact_installs: mpsc::Receiver<(
        brood_protocol::ArtifactManifest,
        std::path::PathBuf,
        tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    )>,
    dynamic_backends: DynamicBackendHandle,
    resize_ready_sender: mpsc::SyncSender<(u64, u64)>,
    resize_ready_receiver: mpsc::Receiver<(u64, u64)>,
    actor_direct_inputs: mpsc::Receiver<ActorInputEvent>,
    server_inputs: mpsc::SyncSender<brood_protocol::InputEvent>,
    clipboard_sender: mpsc::SyncSender<(u64, u64, String)>,
    clipboard_events: mpsc::Receiver<(u64, u64, String)>,
) -> Result<()> {
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(brood_cli::global_config_path);
    let init_actor = if args.no_init {
        None
    } else if let Some(path) = args.init_actor.clone() {
        Some(path)
    } else {
        brood_cli::realm_init_actor_path(&config_path, brood_cli::RealmKind::Client)?
    };
    let actor_specs = args.wasm_actors.clone();
    let configured_policy = if args.wasm_key_policy.is_none() {
        brood_cli::client_key_policy_selection(&config_path)?
    } else {
        None
    };
    let key_policy_spec = args
        .wasm_key_policy
        .as_ref()
        .and_then(|spec| {
            let [provider, path] = spec.as_slice() else {
                return None;
            };
            Some((provider.clone(), std::path::PathBuf::from(path)))
        })
        .or_else(|| configured_policy.map(|policy| (policy.provider, policy.path)));
    let canvas_host = Arc::new(CanvasPresentationHost::new(
        presentation.clone(),
        resize_ready_sender,
    ));
    thread::Builder::new()
        .name("brood-gpui-actors".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let Ok(runtime) = runtime else { return };
            runtime.block_on(run_actor_supervisor(
                init_actor,
                actor_specs,
                key_policy_spec,
                presentation,
                actions,
                catalogs,
                control_target,
                bound_session,
                canvas_host,
                resize_ready_receiver,
                actor_inputs,
                backend_actor_messages,
                presentation_actor_messages,
                remote_commands,
                remote_session_completions,
                remote_artifact_installs,
                dynamic_backends,
                actor_direct_inputs,
                server_inputs,
                clipboard_sender,
                clipboard_events,
            ));
            eprintln!("GPUI actor supervisor stopped");
        })
        .context("spawn GPUI actor supervisor")?;
    Ok(())
}

struct LiveActor {
    id: String,
    pane: Option<u64>,
    generation: u64,
    session: Option<(u64, u64)>,
    module_root: std::path::PathBuf,
    actor: ComponentActor,
}

struct LegacyLayoutActor {
    declaration: WasmActorDeclaration,
    generation: LegacyWasmActorGeneration,
    mailbox: MailboxAddress,
}

struct LegacyLayoutRuntime {
    _registry: Arc<tokio::sync::Mutex<Registry>>,
    host_mailbox: Mailbox,
    actors: BTreeMap<String, LegacyLayoutActor>,
}

struct PendingLayoutPublication {
    actor: String,
    lease: Option<brood_wasm::LayoutLeaseToken>,
    revision: Option<u64>,
    layout: brood_wasm::ResolvedLayout,
    rest: Option<brood_wasm::LayoutChildSlot>,
    focus: Option<u64>,
    transaction: Option<(u64, brood_wasm::LayoutLeaseToken, u64, Vec<u64>)>,
}

#[derive(Clone)]
struct RestDelegation {
    parent: String,
    parent_lease: brood_wasm::LayoutLeaseToken,
    child: String,
    panes: Vec<u64>,
    lease: brood_wasm::LayoutLeaseToken,
    revision: u64,
    transform: Option<brood_wasm::LocalTransform>,
    child_layout: Option<brood_wasm::ResolvedLayout>,
}

#[derive(Clone, Copy, PartialEq)]
struct ComposedZone {
    lease: brood_wasm::LayoutLeaseToken,
    id: brood_wasm::LayoutItemId,
    transform: brood_wasm::LocalTransform,
    lease_viewport: brood_wasm::LocalTransform,
    z: f64,
    event_mask: brood_wasm::PointerEventMask,
    cursor: brood_wasm::CursorRole,
}

struct PointerPolicyBinding {
    lease: brood_wasm::LayoutLeaseToken,
    actor: String,
    actor_generation: u64,
    provider: String,
    wasm: WasmPointerPolicy,
}

#[derive(Clone, Copy)]
struct PointerCaptureState {
    interaction: InteractionId,
    zone: ComposedZone,
}

struct PendingResizeNotification {
    actor: String,
    request_id: u64,
    epoch: brood_wasm::ResizeEpoch,
    waiting: BTreeSet<u64>,
    ready: BTreeSet<u64>,
    layout: BTreeMap<u64, brood_wasm::LocalTransform>,
    zones: Vec<ComposedZone>,
    focus: Option<u64>,
    accepted_at: std::time::Instant,
}

struct ClientKeyPolicy {
    wasm: Option<WasmGlobalKeyPolicy>,
    publications: Registry,
    publication_actor: brood_wasm::ActorId,
    _publication_mailbox: Mailbox,
    shortcut_passthrough: bool,
    binding_overrides: BTreeMap<String, Binding>,
    bindings_path: std::path::PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KeyBindingOverride {
    action: String,
    key_code: i32,
    modifiers: i32,
}

#[derive(Deserialize, Serialize)]
struct StoredBindings {
    version: u32,
    bindings: Vec<KeyBindingOverride>,
}

struct KeyPolicyOutcome {
    decision: GlobalKeyDecision,
    host_messages: Vec<HostMessage>,
    actor_messages: Vec<brood_wasm::GlobalKeyActorMessage>,
}

impl ClientKeyPolicy {
    fn load(
        spec: Option<&(String, std::path::PathBuf)>,
        bindings_path: std::path::PathBuf,
    ) -> Result<Self> {
        let wasm = spec
            .map(|(provider, path)| WasmGlobalKeyPolicy::load(path, provider))
            .transpose()?;
        let mut publications = Registry::new();
        let publication_mailbox = publications
            .register_actor("presentation-client-state", 1)
            .context("register key-policy state publisher")?;
        let publication_actor = publication_mailbox.actor_id();
        Ok(Self {
            wasm,
            publications,
            publication_actor,
            _publication_mailbox: publication_mailbox,
            shortcut_passthrough: false,
            binding_overrides: load_binding_overrides(&bindings_path),
            bindings_path,
        })
    }

    fn evaluate(
        &mut self,
        key_code: i32,
        pressed: bool,
        modifiers: i32,
        focused_pane: Option<u64>,
        actors: &BTreeMap<String, LiveActor>,
        managed_panes: &[u64],
    ) -> Result<KeyPolicyOutcome> {
        if self.wasm.is_none() {
            return Ok(KeyPolicyOutcome {
                decision: GlobalKeyDecision::Abstain,
                host_messages: Vec::new(),
                actor_messages: Vec::new(),
            });
        }
        let panes = actors
            .values()
            .filter_map(|actor| actor.pane)
            .collect::<Vec<_>>();
        let snapshot = serde_json::json!({
            "focused_pane": focused_pane,
            "panes": panes,
            "managed_panes": managed_panes,
            "app_panes": [],
            "bindings": self.binding_overrides(),
        });
        let publication = self.publications.append_default_publication_frame(
            self.publication_actor,
            serde_json::to_vec(&snapshot)?,
        )?;
        let input = GlobalKeyPolicyInput::new(
            key_code,
            if pressed {
                ABI_KEY_PRESSED
            } else {
                ABI_KEY_RELEASED
            },
            modifiers,
            ABI_INPUT_MODE_PASSTHROUGH,
            false,
        );
        let outcome = self
            .wasm
            .as_mut()
            .context("key policy disappeared during evaluation")?
            .handle_key(GlobalKeyPolicyInvocation::new(input, Some(&publication)))?;
        let mut host_messages = Vec::new();
        let mut actor_messages = Vec::new();
        for message in outcome.messages {
            if message.actor == brood_wasm::HOST_ACTOR_NAME {
                host_messages.push(serde_json::from_slice(&message.payload)?);
            } else {
                actor_messages.push(message);
            }
        }
        let was_passthrough = self.shortcut_passthrough;
        let mut changed_passthrough = false;
        host_messages.retain(|message| match message {
            HostMessage::ToggleShortcutPassthrough => {
                self.shortcut_passthrough = !self.shortcut_passthrough;
                changed_passthrough = true;
                false
            }
            HostMessage::SetShortcutPassthrough { enabled } => {
                self.shortcut_passthrough = *enabled;
                changed_passthrough = true;
                false
            }
            _ => true,
        });
        if was_passthrough && !changed_passthrough {
            host_messages.clear();
            actor_messages.clear();
            return Ok(KeyPolicyOutcome {
                decision: GlobalKeyDecision::Forward,
                host_messages,
                actor_messages,
            });
        }
        Ok(KeyPolicyOutcome {
            decision: outcome.decision,
            host_messages,
            actor_messages,
        })
    }

    fn binding_overrides(&self) -> Vec<KeyBindingOverride> {
        self.binding_overrides
            .iter()
            .map(|(action, binding)| KeyBindingOverride {
                action: action.clone(),
                key_code: binding.key_code,
                modifiers: binding.modifiers,
            })
            .collect()
    }

    fn set_binding(&mut self, action_id: &str, binding: Binding) -> Result<()> {
        anyhow::ensure!(binding.is_valid(), "key binding is invalid");
        let target = action(action_id)
            .with_context(|| format!("key binding action {action_id:?} is unknown"))?;
        let old = self.effective_binding(target.id);
        let conflict = ACTIONS
            .iter()
            .find(|candidate| {
                candidate.id != target.id && self.effective_binding(candidate.id) == binding
            })
            .map(|candidate| candidate.id);
        self.set_override(target.id, binding);
        if let Some(conflict) = conflict {
            self.set_override(conflict, old);
        }
        let parent = self
            .bindings_path
            .parent()
            .context("keybindings path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&StoredBindings {
            version: 1,
            bindings: self.binding_overrides(),
        })?;
        let temporary = self
            .bindings_path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(temporary, &self.bindings_path)?;
        Ok(())
    }

    fn effective_binding(&self, action_id: &str) -> Binding {
        self.binding_overrides
            .get(action_id)
            .copied()
            .or_else(|| action(action_id).map(|action| action.default_binding))
            .unwrap_or(Binding::new(0, 0))
    }

    fn set_override(&mut self, action_id: &str, binding: Binding) {
        if let Some(action) = action(action_id) {
            if binding == action.default_binding {
                self.binding_overrides.remove(action_id);
            } else {
                self.binding_overrides
                    .insert(action_id.to_string(), binding);
            }
        }
    }
}

fn load_binding_overrides(path: &std::path::Path) -> BTreeMap<String, Binding> {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(stored) = serde_json::from_slice::<StoredBindings>(&bytes) else {
        eprintln!("ignored malformed global keybindings {}", path.display());
        return BTreeMap::new();
    };
    if stored.version != 1 {
        return BTreeMap::new();
    }
    stored
        .bindings
        .into_iter()
        .filter_map(|candidate| {
            let binding = Binding::new(candidate.key_code, candidate.modifiers);
            (binding.is_valid() && action(&candidate.action).is_some())
                .then_some((candidate.action, binding))
        })
        .collect()
}

impl LegacyLayoutRuntime {
    async fn load(specs: &[(String, std::path::PathBuf)]) -> Result<Self> {
        let mut registry = Registry::new();
        let host_mailbox = registry
            .register_host_mailbox(64)
            .context("register legacy presentation host mailbox")?;
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let mut actors = BTreeMap::new();
        for (id, path) in specs {
            let artifact = WasmPluginArtifact::load(path)
                .with_context(|| format!("inspect legacy presentation actor {}", path.display()))?;
            let WasmPluginArtifact::Legacy(artifact) = artifact else {
                continue;
            };
            let declaration = artifact
                .inventory
                .actors
                .into_iter()
                .find(|candidate| candidate.name == *id)
                .with_context(|| format!("{} does not declare actor {id:?}", path.display()))?;
            let (registration, mailbox) = registry.lock().await.prepare_actor(id.clone(), 64)?;
            let actor = WasmActor::load_named(path, mailbox, &declaration.export_name)
                .await
                .with_context(|| format!("load legacy presentation actor {id:?}"))?;
            let actor_id = actor.actor_id();
            let mailbox = registry
                .lock()
                .await
                .activate_prepared_actor_with_address(registration, &[])?;
            let mut actor_ids = BTreeMap::new();
            actor_ids.insert(id.clone(), actor_id);
            let runtime = tokio::runtime::Handle::current();
            let generation = LegacyWasmActorGeneration::start(
                &runtime,
                vec![(declaration.clone(), actor)],
                actor_ids,
                Arc::clone(&registry),
                path,
                |_event: LegacyWasmRuntimeEvent| {},
            );
            eprintln!(
                "started presentation WASM actor id={id} kind=core path={}",
                path.display()
            );
            actors.insert(
                id.clone(),
                LegacyLayoutActor {
                    declaration,
                    generation,
                    mailbox,
                },
            );
        }
        Ok(Self {
            _registry: registry,
            host_mailbox,
            actors,
        })
    }

    fn send(&self, actor: &str, payload: Vec<u8>) -> Result<()> {
        self.actors
            .get(actor)
            .with_context(|| format!("legacy presentation actor {actor:?} is unavailable"))?
            .mailbox
            .try_post(payload)
    }

    fn drain_host_messages(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut messages = Vec::new();
        while let Some(message) = self.host_mailbox.try_read_envelope()? {
            let Some(sender) = message.sender else {
                continue;
            };
            let Some(name) = self.actors.iter().find_map(|(name, actor)| {
                actor
                    .generation
                    .actor_ids()
                    .values()
                    .any(|id| *id == sender)
                    .then(|| name.clone())
            }) else {
                continue;
            };
            messages.push((name, message.payload));
        }
        Ok(messages)
    }

    fn layout_actor(&self, name: &str) -> bool {
        self.actors.get(name).is_some_and(|actor| {
            actor
                .declaration
                .subscribes_to(WasmActorEventKind::LayoutState)
        })
    }
}

async fn run_actor_supervisor(
    init_actor: Option<std::path::PathBuf>,
    actor_specs: Vec<String>,
    key_policy_spec: Option<(String, std::path::PathBuf)>,
    presentation: PresentationHandle,
    actions: mpsc::Receiver<brood_protocol::managed_ui::UiAction>,
    catalogs: mpsc::Receiver<ClientCatalog>,
    control_target: Option<brood_cli::control::ControlTarget>,
    bound_session: Option<(u64, u64)>,
    canvas_host: Arc<CanvasPresentationHost>,
    resize_ready: mpsc::Receiver<(u64, u64)>,
    actor_inputs: mpsc::Receiver<brood_protocol::InputEvent>,
    backend_actor_messages: mpsc::SyncSender<BackendActorMessage>,
    presentation_actor_messages: mpsc::Receiver<BackendActorMessage>,
    remote_commands: Arc<Mutex<BTreeMap<u64, remote::RemoteBackendCommandLane>>>,
    remote_session_completions: mpsc::Receiver<(String, SessionCommandCompletion)>,
    remote_artifact_installs: mpsc::Receiver<(
        brood_protocol::ArtifactManifest,
        std::path::PathBuf,
        tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    )>,
    dynamic_backends: DynamicBackendHandle,
    actor_direct_inputs: mpsc::Receiver<ActorInputEvent>,
    server_inputs: mpsc::SyncSender<brood_protocol::InputEvent>,
    clipboard_sender: mpsc::SyncSender<(u64, u64, String)>,
    clipboard_events: mpsc::Receiver<(u64, u64, String)>,
) {
    let mut key_policy = match ClientKeyPolicy::load(
        key_policy_spec.as_ref(),
        brood_cli::global_keybindings_path(),
    ) {
        Ok(policy) => policy,
        Err(error) => {
            eprintln!("could not start presentation key policy: {error:#}");
            return;
        }
    };
    let legacy_specs = key_policy_spec.clone().into_iter().collect::<Vec<_>>();
    let pointer_policy_module = key_policy_spec.as_ref().map(|(_, path)| path.clone());
    let mut legacy_runtime = match LegacyLayoutRuntime::load(&legacy_specs).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start legacy presentation providers: {error:#}");
            return;
        }
    };
    let mut actors = BTreeMap::<String, LiveActor>::new();
    let mut next_pane = 1_u64 << 63;
    let mut next_layout_revision = 1_u64;
    let mut next_render_token = 1_u64;
    let render_clock_origin = std::time::Instant::now();
    let mut last_render_hint = std::time::Instant::now();
    let mut focused_pane = None;
    let mut available_windows = Vec::<CatalogWireWindow>::new();
    let mut presentation_size = (1280_u32, 800_u32, 120_u32);
    let mut presentation_refresh_millihz = 60_000_u32;
    let mut committed_layout = BTreeMap::new();
    let mut committed_layout_revision = 0_u64;
    let mut committed_zones = Vec::<ComposedZone>::new();
    let mut pointer_policies = BTreeMap::<u64, PointerPolicyBinding>::new();
    let mut pointer_capture: Option<PointerCaptureState> = None;
    let mut next_pointer_interaction = 1_u64;
    let mut pointer_event_sequence = 0_u64;
    let mut pointer_cursor = brood_wasm::CursorRole::Default;
    let mut pending_resizes = BTreeMap::<u64, PendingResizeNotification>::new();
    let mut root_layout: Option<brood_wasm::ResolvedLayout> = None;
    let mut rest_delegations = BTreeMap::<u64, RestDelegation>::new();
    let mut next_layout_lease = 2_u64;
    let mut backend_catalogues = BTreeMap::<u64, ClientCatalog>::new();
    let mut root_layout_actor = None;
    let mut layout_state_dirty = false;
    let mut suppress_next_text = false;
    let mut pending_actor_inputs = std::collections::VecDeque::<(u64, PresentationEvent)>::new();
    let mut pure_unsupported = BTreeSet::<u64>::new();
    let mut latest_pure_token = BTreeMap::<u64, u64>::new();
    let mut pending_pure = BTreeMap::<
        u64,
        (
            u64,
            u64,
            u64,
            u64,
            u64,
            tokio::sync::oneshot::Receiver<
                std::result::Result<brood_wasm::PureRenderResult, PureRenderError>,
            >,
        ),
    >::new();
    if let Some(path) = init_actor {
        match load_actor(
            "brood.client-init",
            &path,
            None,
            bound_session,
            true,
            &[],
            canvas_host.clone(),
        )
        .await
        {
            Ok(actor) => {
                eprintln!(
                    "started presentation WASM actor id={} generation={} scope=global",
                    actor.id, actor.generation
                );
                actors.insert(actor.id.clone(), actor);
            }
            Err(error) => {
                eprintln!(
                    "could not start presentation init actor {}: {error:#}",
                    path.display()
                );
            }
        }
    }
    for spec in actor_specs {
        let Some((id, path)) = spec.split_once('=') else {
            continue;
        };
        let pane = next_pane;
        next_pane = next_pane.saturating_add(1);
        if let Ok(actor) = load_actor(
            id.split('*').next().unwrap_or(id),
            std::path::Path::new(path),
            Some(pane),
            None,
            false,
            &[],
            canvas_host.clone(),
        )
        .await
        {
            actors.insert(actor.id.clone(), actor);
        }
    }
    loop {
        while let Ok((pane, epoch)) = resize_ready.try_recv() {
            if let Some(transaction) = pending_resizes.get_mut(&epoch)
                && transaction.waiting.contains(&pane)
            {
                transaction.ready.insert(pane);
            }
        }
        let completed_epochs = pending_resizes
            .iter()
            .filter_map(|(epoch, transaction)| {
                let ready = transaction.ready == transaction.waiting;
                let expired =
                    transaction.accepted_at.elapsed() >= std::time::Duration::from_millis(500);
                (ready || expired).then_some((*epoch, !ready))
            })
            .collect::<Vec<_>>();
        for (epoch, degraded) in completed_epochs {
            let Some(transaction) = pending_resizes.remove(&epoch) else {
                continue;
            };
            if presentation
                .try_send(PresentationCommand::CommitResizeCandidate {
                    epoch,
                    revision: transaction.epoch.target_layout_revision,
                    panes: transaction.layout.clone(),
                })
                .is_err()
            {
                return;
            }
            committed_layout = transaction.layout.clone();
            committed_zones.clone_from(&transaction.zones);
            committed_layout_revision = transaction.epoch.target_layout_revision;
            if let Some(pane) = transaction.focus {
                update_actor_focus(&actors, &mut focused_pane, pane);
                let _ = server_inputs.try_send(brood_protocol::InputEvent::WindowFocus { pane });
            }
            let missing = transaction
                .waiting
                .difference(&transaction.ready)
                .copied()
                .collect::<Vec<_>>();
            let notification = if degraded {
                brood_wasm::SynchronizedLayoutNotification::Degraded(
                    brood_wasm::SynchronizedLayoutDegraded {
                        request_id: transaction.request_id,
                        resize_epoch: transaction.epoch,
                        missing_panes: missing.clone(),
                    },
                )
            } else {
                brood_wasm::SynchronizedLayoutNotification::Completed(
                    brood_wasm::SynchronizedLayoutCompleted {
                        request_id: transaction.request_id,
                        resize_epoch: transaction.epoch,
                    },
                )
            };
            if let Ok(body) = serde_json::to_vec(&notification) {
                if let Some(actor) = actors.get(&transaction.actor) {
                    drop(
                        actor
                            .actor
                            .try_send_message("layout-host".to_string(), body),
                    );
                } else if let Err(error) = legacy_runtime.send(&transaction.actor, body) {
                    eprintln!(
                        "could not complete synchronized layout actor={}: {error:#}",
                        transaction.actor
                    );
                }
            }
            eprintln!(
                "synchronized layout ready actor={} request={} pane_ready={}/{} staged={} missing={missing:?} degraded={degraded}",
                transaction.actor,
                transaction.request_id,
                transaction.ready.len(),
                transaction.waiting.len(),
                transaction.ready.len(),
            );
        }
        while let Ok(message) = presentation_actor_messages.try_recv() {
            if let Some(actor) = actors.get(&message.recipient)
                && actor_accepts_backend_message(actor.session, message.realm)
            {
                let qualified_sender = match message.realm {
                    BackendActorRealm::Daemon => format!("daemon:1:{}", message.sender),
                    BackendActorRealm::Session(_) => format!("session:1:{}", message.sender),
                };
                eprintln!(
                    "presentation actor message realm={:?} sender={} recipient={} bytes={}",
                    message.realm,
                    message.sender,
                    message.recipient,
                    message.body.len()
                );
                drop(actor.actor.try_send_message(qualified_sender, message.body));
            }
        }
        while let Ok((actor_id, completion)) = remote_session_completions.try_recv() {
            if let Some(actor) = actors.get(&actor_id) {
                drop(
                    actor
                        .actor
                        .try_send_session_event(SessionEvent::CommandCompleted(completion)),
                );
            }
        }
        while let Ok((manifest, path, completion)) = remote_artifact_installs.try_recv() {
            let result = if actors.contains_key(&manifest.id) {
                Err(format!("client actor {} is already installed", manifest.id))
            } else {
                let pane = next_pane;
                match load_actor(
                    &manifest.id,
                    &path,
                    Some(pane),
                    None,
                    false,
                    &manifest.capabilities,
                    canvas_host.clone(),
                )
                .await
                {
                    Ok(actor) => {
                        next_pane = next_pane.saturating_add(1);
                        eprintln!(
                            "installed remote client actor id={} generation={} pane={pane}",
                            actor.id, actor.generation
                        );
                        actors.insert(actor.id.clone(), actor);
                        layout_state_dirty = true;
                        Ok(())
                    }
                    Err(error) => Err(format!(
                        "load remote client actor {}: {error:#}",
                        manifest.id
                    )),
                }
            };
            let _ = completion.send(result);
        }
        let actor_input_ready = if let Some((pane, first)) = pending_actor_inputs.pop_front() {
            if let Some(actor) = actors.values().find(|actor| actor.pane == Some(pane)) {
                let mut events = vec![(first, None)];
                while events.len() < 16
                    && pending_actor_inputs
                        .front()
                        .is_some_and(|(candidate, _)| *candidate == pane)
                {
                    if let Some((_, event)) = pending_actor_inputs.pop_front() {
                        events.push((event, None));
                    }
                }
                match actor.actor.try_send_presentation_events(events) {
                    Ok(())
                    | Err(brood_wasm::ComponentActorPresentationEnvelopeSendError::Closed(_)) => {
                        true
                    }
                    Err(brood_wasm::ComponentActorPresentationEnvelopeSendError::Full(events)) => {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                            eprintln!(
                                "presentation actor input backpressure pane={pane} batch={} pending={}",
                                events.len(),
                                pending_actor_inputs.len()
                            );
                        }
                        for (event, _) in events.into_iter().rev() {
                            pending_actor_inputs.push_front((pane, event));
                        }
                        false
                    }
                    Err(
                        brood_wasm::ComponentActorPresentationEnvelopeSendError::InvalidSize(_)
                        | brood_wasm::ComponentActorPresentationEnvelopeSendError::InvariantViolation,
                    ) => {
                        eprintln!("presentation actor input queue invariant failed pane={pane}");
                        true
                    }
                }
            } else {
                true
            }
        } else {
            true
        };
        if actor_input_ready {
            for _ in 0..512 {
                let Ok(input) = actor_direct_inputs.try_recv() else {
                    break;
                };
                // Pointer routing can establish the initial focused pane. Key/text events with no
                // focus naturally fall through to the session lane or an unowned pane.
                let pane = focused_pane.unwrap_or(0);
                let event = match input {
                    ActorInputEvent::Key {
                        keycode,
                        mut logical_key,
                        pressed,
                        modifiers,
                    } => {
                        logical_key = normalize_logical_key(&logical_key);
                        let managed_panes = available_windows
                            .iter()
                            .map(|window| window.pane)
                            .collect::<Vec<_>>();
                        let outcome = key_policy.evaluate(
                            // The policy ABI uses XKB/X11 key codes, while GPUI's Wayland backend
                            // reports Linux evdev codes after removing XKB's conventional offset.
                            i32::try_from(keycode.saturating_add(8)).unwrap_or(i32::MAX),
                            pressed,
                            i32::try_from(modifiers).unwrap_or(i32::MAX),
                            focused_pane,
                            &actors,
                            &managed_panes,
                        );
                        let outcome = match outcome {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                eprintln!("presentation key-policy evaluation failed: {error:#}");
                                KeyPolicyOutcome {
                                    decision: GlobalKeyDecision::Abstain,
                                    host_messages: Vec::new(),
                                    actor_messages: Vec::new(),
                                }
                            }
                        };
                        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                            eprintln!(
                                "presentation key input keycode={keycode} logical={logical_key:?} pressed={pressed} modifiers={modifiers} decision={:?}",
                                outcome.decision
                            );
                        }
                        for message in outcome.actor_messages {
                            if let Some(actor) = actors.get(&message.actor) {
                                eprintln!(
                                    "sent key-policy actor message actor={} bytes={}",
                                    message.actor,
                                    message.payload.len()
                                );
                                drop(
                                    actor.actor.try_send_message(
                                        "key-policy".to_string(),
                                        message.payload,
                                    ),
                                );
                            }
                        }
                        for message in outcome.host_messages {
                            match message {
                                HostMessage::FocusPane { pane } => {
                                    update_actor_focus(&actors, &mut focused_pane, pane);
                                    let _ = server_inputs
                                        .try_send(brood_protocol::InputEvent::WindowFocus { pane });
                                }
                                HostMessage::SetKeybinding {
                                    action,
                                    key_code,
                                    modifiers,
                                } => {
                                    if let Err(error) = key_policy
                                        .set_binding(&action, Binding::new(key_code, modifiers))
                                    {
                                        eprintln!("rejected keybinding update: {error:#}");
                                    }
                                }
                                message => execute_host_operation(
                                    control_target.as_ref(),
                                    &presentation,
                                    &clipboard_sender,
                                    &remote_commands,
                                    &dynamic_backends,
                                    0,
                                    message,
                                    focused_pane,
                                ),
                            }
                        }
                        if outcome.decision == GlobalKeyDecision::Consume {
                            suppress_next_text |= pressed;
                            None
                        } else if actors.values().any(|actor| actor.pane == Some(pane))
                            && canvas_host.accepts_raw_keyboard(pane)
                        {
                            Some(PresentationEvent::Key {
                                pane,
                                physical_key: keycode,
                                logical_key,
                                pressed,
                                repeat: false,
                                modifiers,
                            })
                        } else {
                            let _ =
                                server_inputs.try_send(brood_protocol::InputEvent::KeyboardKey {
                                    keycode,
                                    pressed,
                                });
                            None
                        }
                    }
                    ActorInputEvent::Text(_text) if suppress_next_text => {
                        suppress_next_text = false;
                        None
                    }
                    ActorInputEvent::Text(text) if canvas_host.accepts_raw_keyboard(pane) => {
                        Some(PresentationEvent::Text { pane, text })
                    }
                    ActorInputEvent::Paste(text) if canvas_host.accepts_raw_keyboard(pane) => {
                        Some(PresentationEvent::Paste { pane, text })
                    }
                    ActorInputEvent::Text(_) | ActorInputEvent::Paste(_) => None,
                    ActorInputEvent::PointerMotion { x, y, modifiers } => {
                        route_pointer_input(
                            NativePointerInput::Motion { x, y, modifiers },
                            &actors,
                            &legacy_runtime,
                            &committed_layout,
                            &committed_zones,
                            presentation_size,
                            &mut pointer_policies,
                            &mut pointer_capture,
                            &mut next_pointer_interaction,
                            &mut pointer_event_sequence,
                            &mut focused_pane,
                            &presentation,
                            &mut pointer_cursor,
                            &canvas_host,
                            &server_inputs,
                        );
                        None
                    }
                    ActorInputEvent::PointerButton {
                        x,
                        y,
                        button,
                        pressed,
                        modifiers,
                    } => {
                        route_pointer_input(
                            NativePointerInput::Button {
                                x,
                                y,
                                button,
                                pressed,
                                modifiers,
                            },
                            &actors,
                            &legacy_runtime,
                            &committed_layout,
                            &committed_zones,
                            presentation_size,
                            &mut pointer_policies,
                            &mut pointer_capture,
                            &mut next_pointer_interaction,
                            &mut pointer_event_sequence,
                            &mut focused_pane,
                            &presentation,
                            &mut pointer_cursor,
                            &canvas_host,
                            &server_inputs,
                        );
                        None
                    }
                    ActorInputEvent::PointerScroll {
                        x,
                        y,
                        horizontal,
                        vertical,
                        modifiers,
                    } => {
                        route_pointer_input(
                            NativePointerInput::Scroll {
                                x,
                                y,
                                horizontal,
                                vertical,
                                modifiers,
                            },
                            &actors,
                            &legacy_runtime,
                            &committed_layout,
                            &committed_zones,
                            presentation_size,
                            &mut pointer_policies,
                            &mut pointer_capture,
                            &mut next_pointer_interaction,
                            &mut pointer_event_sequence,
                            &mut focused_pane,
                            &presentation,
                            &mut pointer_cursor,
                            &canvas_host,
                            &server_inputs,
                        );
                        None
                    }
                };
                if let Some(event) = event {
                    pending_actor_inputs.push_back((pane, event));
                }
            }
        }
        while let Ok((generation, pane, text)) = clipboard_events.try_recv() {
            if let Some(actor) = actors
                .values()
                .find(|actor| actor.generation == generation && actor.pane == Some(pane))
            {
                drop(
                    actor
                        .actor
                        .try_send_presentation_event(PresentationEvent::Paste { pane, text }),
                );
            }
        }
        while let Ok(input) = actor_inputs.try_recv() {
            match input {
                brood_protocol::InputEvent::WindowFocus { pane } => {
                    update_actor_focus(&actors, &mut focused_pane, pane);
                }
                brood_protocol::InputEvent::PresentationSize {
                    width,
                    height,
                    refresh_millihz,
                    scale_120,
                } => {
                    presentation_size = (width, height, scale_120);
                    presentation_refresh_millihz = refresh_millihz.max(1_000);
                    layout_state_dirty = true;
                    resize_actor_panes(
                        &actors,
                        &committed_layout,
                        presentation_size,
                        None,
                        &server_inputs,
                    );
                }
                brood_protocol::InputEvent::PointerMotion { position } => {
                    send_pane_event(
                        &actors,
                        position.pane,
                        PresentationEvent::PointerMoved {
                            pane: position.pane,
                            x: position.x as f32,
                            y: position.y as f32,
                        },
                    );
                }
                brood_protocol::InputEvent::PointerButton {
                    position,
                    button,
                    pressed,
                } => {
                    send_pane_event(
                        &actors,
                        position.pane,
                        PresentationEvent::PointerButton {
                            pane: position.pane,
                            x: position.x as f32,
                            y: position.y as f32,
                            button,
                            pressed,
                            modifiers: 0,
                        },
                    );
                }
                brood_protocol::InputEvent::PointerAxis {
                    position,
                    horizontal,
                    vertical,
                } => {
                    send_pane_event(
                        &actors,
                        position.pane,
                        PresentationEvent::PointerScroll {
                            pane: position.pane,
                            x: position.x as f32,
                            y: position.y as f32,
                            horizontal: horizontal as f32,
                            vertical: vertical as f32,
                            modifiers: 0,
                        },
                    );
                }
                brood_protocol::InputEvent::PointerWheel {
                    position,
                    horizontal_v120,
                    vertical_v120,
                } => {
                    send_pane_event(
                        &actors,
                        position.pane,
                        PresentationEvent::PointerScroll {
                            pane: position.pane,
                            x: position.x as f32,
                            y: position.y as f32,
                            horizontal: horizontal_v120 as f32 / 120.0,
                            vertical: vertical_v120 as f32 / 120.0,
                            modifiers: 0,
                        },
                    );
                }
                brood_protocol::InputEvent::PresentationFocus { focused } => {
                    if let Some(pane) = focused_pane {
                        send_pane_event(
                            &actors,
                            pane,
                            PresentationEvent::PaneFocused { pane, focused },
                        );
                    }
                }
                _ => {}
            }
        }
        let mut pending_token = pending_pure.first_key_value().map(|(token, _)| *token);
        while let Some(token) = pending_token {
            pending_token = pending_pure
                .range((std::ops::Bound::Excluded(token), std::ops::Bound::Unbounded))
                .next()
                .map(|(token, _)| *token);
            let outcome = pending_pure
                .get_mut(&token)
                .map(|(_, _, _, _, _, response)| response.try_recv());
            let Some(outcome) = outcome else { continue };
            match outcome {
                Ok(Ok(result)) => {
                    let Some((
                        generation,
                        pane,
                        geometry_revision,
                        dirty_revision,
                        submission_deadline_ns,
                        _response,
                    )) = pending_pure.remove(&token)
                    else {
                        continue;
                    };
                    let mut draws = Vec::new();
                    let mut geometry_output = false;
                    let current_geometry_revision = pending_resizes
                        .iter()
                        .rev()
                        .find(|(_, resize)| resize.waiting.contains(&pane))
                        .map_or_else(|| committed_layout_revision.max(1), |(epoch, _)| *epoch);
                    let mut valid = latest_pure_token.get(&generation) == Some(&token)
                        && result.token == RenderToken(token)
                        && result.dirty_revision == dirty_revision
                        && geometry_revision == current_geometry_revision
                        && u64::try_from(render_clock_origin.elapsed().as_nanos())
                            .unwrap_or(u64::MAX)
                            <= submission_deadline_ns
                        && result.panes.len() == 1;
                    for (output_pane, output) in result.panes {
                        valid &= output_pane == pane;
                        match output {
                            PurePaneOutput::Draw(mut draw) => {
                                geometry_output = true;
                                valid &= draw.pane == pane;
                                if pending_resizes
                                    .get(&geometry_revision)
                                    .is_some_and(|resize| resize.waiting.contains(&pane))
                                {
                                    draw.resize_epoch = Some(geometry_revision);
                                }
                                draws.push(draw);
                            }
                            PurePaneOutput::ReplayRetainedPacket(revision) => {
                                geometry_output = true;
                                valid &= canvas_host.canvas_revision(generation) == Some(revision);
                                if pending_resizes
                                    .get(&geometry_revision)
                                    .is_some_and(|resize| resize.waiting.contains(&pane))
                                {
                                    let _ = canvas_host.reuse_pane_content(
                                        generation,
                                        pane,
                                        geometry_revision,
                                    );
                                }
                            }
                            PurePaneOutput::NoChange => {}
                        }
                    }
                    if pending_resizes
                        .get(&geometry_revision)
                        .is_some_and(|resize| resize.waiting.contains(&pane))
                    {
                        valid &= geometry_output;
                    }
                    if valid {
                        if canvas_host
                            .finish_actor_frame(generation, RenderToken(token), draws)
                            .is_err()
                        {
                            let _ = canvas_host.request_render(generation);
                        }
                        if result.request_next_frame {
                            let _ = canvas_host.request_render(generation);
                        }
                    } else if latest_pure_token.get(&generation) == Some(&token) {
                        let _ = canvas_host.finish_actor_frame(
                            generation,
                            RenderToken(token),
                            Vec::new(),
                        );
                        let _ = canvas_host.request_render(generation);
                    }
                }
                Ok(Err(PureRenderError::Unsupported)) => {
                    let Some((generation, pane, _geometry, _dirty, _deadline, _response)) =
                        pending_pure.remove(&token)
                    else {
                        continue;
                    };
                    pure_unsupported.insert(generation);
                    if latest_pure_token.get(&generation) == Some(&token)
                        && let Some(actor) =
                            actors.values().find(|actor| actor.generation == generation)
                    {
                        drop(actor.actor.try_send_presentation_event(
                            PresentationEvent::RenderHint(RenderHint {
                                token: RenderToken(token),
                                target_presentation_time_ns: 0,
                                submission_deadline_ns: u64::MAX,
                                relevant_panes: vec![pane],
                                event_watermark: 0,
                            }),
                        ));
                    }
                }
                Ok(Err(_)) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    if let Some((generation, _pane, _geometry, _dirty, _deadline, _response)) =
                        pending_pure.remove(&token)
                    {
                        if latest_pure_token.get(&generation) == Some(&token) {
                            let _ = canvas_host.finish_actor_frame(
                                generation,
                                RenderToken(token),
                                Vec::new(),
                            );
                            let _ = canvas_host.request_render(generation);
                        }
                    }
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    let overdue =
                        pending_pure
                            .get(&token)
                            .is_some_and(|(_, _, _, _, deadline, _)| {
                                u64::try_from(render_clock_origin.elapsed().as_nanos())
                                    .unwrap_or(u64::MAX)
                                    > *deadline
                            });
                    if overdue
                        && let Some((generation, _, _, _, _, _)) = pending_pure.remove(&token)
                        && latest_pure_token.get(&generation) == Some(&token)
                    {
                        let _ = canvas_host.finish_actor_frame(
                            generation,
                            RenderToken(token),
                            Vec::new(),
                        );
                        let _ = canvas_host.request_render(generation);
                    }
                }
            }
        }
        let frame_interval_ns = 1_000_000_000_000_u64
            .checked_div(u64::from(presentation_refresh_millihz))
            .unwrap_or(16_666_667)
            .max(1_000_000);
        let submission_budget_ns = frame_interval_ns.saturating_mul(4) / 5;
        if last_render_hint.elapsed() >= std::time::Duration::from_nanos(frame_interval_ns) {
            let now = u64::try_from(render_clock_origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
            for (generation, pane, dirty_revision) in canvas_host.render_targets() {
                if let Some(actor) = actors.values().find(|actor| actor.generation == generation) {
                    if pure_unsupported.contains(&generation) {
                        drop(actor.actor.try_send_presentation_event(
                            PresentationEvent::RenderHint(RenderHint {
                                token: RenderToken(next_render_token),
                                target_presentation_time_ns: now.saturating_add(frame_interval_ns),
                                submission_deadline_ns: now.saturating_add(submission_budget_ns),
                                relevant_panes: vec![pane],
                                event_watermark: 0,
                            }),
                        ));
                    } else {
                        let pending_geometry = pending_resizes
                            .iter()
                            .rev()
                            .find(|(_, resize)| resize.waiting.contains(&pane))
                            .map(|(epoch, resize)| (*epoch, &resize.layout));
                        let geometry_revision = pending_geometry
                            .map_or_else(|| committed_layout_revision.max(1), |(epoch, _)| epoch);
                        let geometry =
                            pending_geometry.map_or(&committed_layout, |(_, geometry)| geometry);
                        let (logical_width, logical_height) = geometry.get(&pane).map_or(
                            (presentation_size.0 as f32, presentation_size.1 as f32),
                            |transform| {
                                (
                                    (transform.size.u * 0.5 * f64::from(presentation_size.0))
                                        as f32,
                                    (transform.size.v * 0.5 * f64::from(presentation_size.1))
                                        as f32,
                                )
                            },
                        );
                        let scale = presentation_size.2 as f32 / 120.0;
                        let request = PureRenderRequest {
                            token: RenderToken(next_render_token),
                            reduced_watermark: 0,
                            dirty_revision,
                            target_presentation_time_ns: now.saturating_add(frame_interval_ns),
                            submission_deadline_ns: now.saturating_add(submission_budget_ns),
                            panes: vec![RenderPaneTarget {
                                pane,
                                geometry_revision,
                                logical_width,
                                logical_height,
                                physical_width: (logical_width * scale).round() as u32,
                                physical_height: (logical_height * scale).round() as u32,
                                scale,
                                visible: true,
                                focused: focused_pane == Some(pane),
                            }],
                        };
                        match actor.actor.try_request_pure_render(request) {
                            Ok(response) => {
                                latest_pure_token.insert(generation, next_render_token);
                                pending_pure.insert(
                                    next_render_token,
                                    (
                                        generation,
                                        pane,
                                        geometry_revision,
                                        dirty_revision,
                                        now.saturating_add(submission_budget_ns),
                                        response,
                                    ),
                                );
                            }
                            Err(_request) => {
                                let _ = canvas_host.request_render(generation);
                            }
                        }
                    }
                }
            }
            next_render_token = next_render_token.saturating_add(1);
            last_render_hint = std::time::Instant::now();
        }
        while let Ok(catalog) = catalogs.try_recv() {
            eprintln!(
                "presentation catalogue backend={} revision={} active={} sessions={} windows={}",
                catalog.sessions.backend_id,
                catalog.sessions.revision,
                catalog.sessions.active,
                catalog.sessions.sessions.len(),
                catalog.windows.len()
            );
            backend_catalogues.insert(catalog.sessions.backend_id, catalog.clone());
            available_windows = backend_catalogues
                .values()
                .flat_map(|catalog| catalog.windows.iter().cloned())
                .collect();
            for actor in actors.values() {
                drop(
                    actor
                        .actor
                        .try_send_session_event(SessionEvent::Snapshot(catalog.sessions.clone())),
                );
            }
            publish_backend_catalogue(
                &actors,
                &backend_catalogues,
                dynamic_backends.can_connect_remote,
                dynamic_backends.deployer.is_some(),
            );
            layout_state_dirty = true;
        }
        while let Ok(action) = actions.try_recv() {
            if let Some(actor) = actors
                .values()
                .find(|actor| actor.generation == action.actor_generation)
                && let Ok(body) = serde_json::to_vec(&action)
            {
                drop(
                    actor
                        .actor
                        .try_send_message("managed-ui-host".to_string(), body),
                );
            }
        }
        let ids = actors.keys().cloned().collect::<Vec<_>>();
        let mut starts = Vec::new();
        let mut stops = Vec::new();
        let mut messages = Vec::new();
        let mut publications = Vec::new();
        let mut session_commands = Vec::new();
        let mut layouts = Vec::<PendingLayoutPublication>::new();
        let mut delegations = Vec::new();
        let mut layout_claims = Vec::new();
        let mut backend_messages = Vec::new();
        let mut focus_requests = Vec::new();
        let mut host_operations = Vec::new();
        let mut pointer_selections = Vec::new();
        let mut catalogue_requested = false;
        for id in ids {
            let Some(live) = actors.get_mut(&id) else {
                continue;
            };
            for _ in 0..128 {
                let Some(event) = live.actor.try_recv_event() else {
                    break;
                };
                match event {
                    ComponentActorEvent::ActorStartRequested {
                        id,
                        path,
                        capabilities,
                        presentation_scope,
                    } => {
                        eprintln!(
                            "presentation actor requested child parent={} id={} path={} scope={:?}",
                            live.id, id, path, presentation_scope
                        );
                        starts.push((
                            live.id.clone(),
                            id,
                            live.module_root.join(path),
                            presentation_scope.session(),
                            capabilities,
                        ));
                    }
                    ComponentActorEvent::ActorStopRequested { id } => {
                        stops.push((live.id.clone(), id))
                    }
                    ComponentActorEvent::LocalMessageRequested { actor, body }
                    | ComponentActorEvent::PresentationMessageRequested { actor, body } => {
                        messages.push((live.id.clone(), actor, body));
                    }
                    ComponentActorEvent::SessionMessageRequested {
                        backend_id,
                        session_id,
                        actor,
                        body,
                    } => backend_messages.push(BackendActorMessage {
                        backend: backend_id,
                        sender: live.id.clone(),
                        realm: BackendActorRealm::Session(session_id),
                        recipient: actor,
                        body,
                    }),
                    ComponentActorEvent::DaemonMessageRequested {
                        backend_id,
                        actor,
                        body,
                    } => backend_messages.push(BackendActorMessage {
                        backend: backend_id,
                        sender: live.id.clone(),
                        realm: BackendActorRealm::Daemon,
                        recipient: actor,
                        body,
                    }),
                    ComponentActorEvent::HostMessageRequested { payload } => {
                        match serde_json::from_slice(&payload) {
                            Ok(HostMessage::PublishManagedUi(publication)) => {
                                if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some() {
                                    eprintln!(
                                        "presentation managed UI publication actor={id} generation={}",
                                        live.generation
                                    );
                                }
                                canvas_host.mark_managed(live.generation);
                                let published_pane = match &publication {
                                    brood_protocol::managed_ui::UiPublication::Snapshot(
                                        document,
                                    ) => document.pane,
                                    brood_protocol::managed_ui::UiPublication::Patch(patch) => {
                                        patch.pane
                                    }
                                    brood_protocol::managed_ui::UiPublication::RemovePane {
                                        pane,
                                    } => *pane,
                                };
                                if live.pane == Some(published_pane) {
                                    publications.push((live.generation, publication));
                                } else {
                                    eprintln!(
                                        "rejected managed UI for pane not owned by actor {id}"
                                    );
                                }
                            }
                            Ok(HostMessage::SetResolvedLayout {
                                lease,
                                revision,
                                layout,
                                rest,
                                focus,
                            }) => {
                                layouts.push(PendingLayoutPublication {
                                    actor: live.id.clone(),
                                    lease,
                                    revision,
                                    layout,
                                    rest,
                                    focus,
                                    transaction: None,
                                });
                            }
                            Ok(HostMessage::SubmitSynchronizedLayout(request)) => {
                                let transaction = (
                                    request.request_id,
                                    request.lease,
                                    request.base_layout_revision,
                                    request.wait_for_panes,
                                );
                                layouts.push(PendingLayoutPublication {
                                    actor: live.id.clone(),
                                    lease: Some(request.lease),
                                    revision: Some(request.base_layout_revision),
                                    layout: request.fragment.layout,
                                    rest: request.fragment.rest,
                                    focus: request.focus,
                                    transaction: Some(transaction),
                                });
                            }
                            Ok(HostMessage::FocusPane { pane }) => focus_requests.push(pane),
                            Ok(HostMessage::BackendSession {
                                backend,
                                request,
                                operation,
                                session,
                                name,
                                directory,
                            }) => {
                                let operation = match operation.as_str() {
                                    "refresh" => Some(SessionOperation::Refresh),
                                    "activate" => Some(SessionOperation::Activate),
                                    "create" => Some(SessionOperation::Create),
                                    "rename" => Some(SessionOperation::Rename),
                                    "close" => Some(SessionOperation::Close),
                                    _ => None,
                                };
                                if let Some(operation) = operation {
                                    session_commands.push((
                                        live.id.clone(),
                                        SessionCommandRequest {
                                            backend_id: backend,
                                            request,
                                            operation,
                                            session,
                                            name,
                                            directory,
                                        },
                                    ));
                                }
                            }
                            Ok(HostMessage::RequestBackendCatalogue) => {
                                catalogue_requested = true;
                            }
                            Ok(HostMessage::ClaimRootLayout) => {
                                eprintln!("presentation root layout claimed actor={}", live.id);
                                layout_claims.push(live.id.clone());
                            }
                            Ok(HostMessage::DelegateLayoutRest {
                                lease,
                                revision,
                                actor,
                                panes,
                            }) => {
                                delegations.push((live.id.clone(), lease, revision, actor, panes))
                            }
                            Ok(HostMessage::SetPointerPolicy { lease, provider }) => {
                                pointer_selections.push((
                                    live.id.clone(),
                                    live.generation,
                                    lease,
                                    provider,
                                ));
                            }
                            Ok(message) => host_operations.push((live.generation, message)),
                            Err(error) => eprintln!(
                                "rejected presentation host message actor={} error={error} payload={}",
                                live.id,
                                String::from_utf8_lossy(&payload)
                            ),
                        }
                    }
                    ComponentActorEvent::SessionCommandRequested(request) => {
                        session_commands.push((live.id.clone(), request));
                    }
                    ComponentActorEvent::DispatcherFailed { message } => {
                        eprintln!("presentation actor {id} failed: {message}");
                    }
                    ComponentActorEvent::WorkerStarted { worker } => {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some() {
                            eprintln!(
                                "presentation actor worker started actor={id} worker={}",
                                worker.raw()
                            );
                        }
                    }
                    ComponentActorEvent::WorkerFinished { worker, output } => {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some() {
                            eprintln!(
                                "presentation actor worker finished actor={id} worker={} bytes={}",
                                worker.raw(),
                                output.len()
                            );
                            if output.first() == Some(&b'{')
                                && let Ok(metrics) = std::str::from_utf8(&output)
                            {
                                eprintln!(
                                    "presentation plugin worker metrics actor={id} {metrics}"
                                );
                            }
                        }
                    }
                    ComponentActorEvent::WorkerFailed { worker, message } => {
                        eprintln!(
                            "presentation actor worker failed actor={id} worker={} message={message}",
                            worker.raw()
                        );
                    }
                    ComponentActorEvent::WorkerCancelled { worker } => {
                        eprintln!(
                            "presentation actor worker cancelled actor={id} worker={}",
                            worker.raw()
                        );
                    }
                    _ => {}
                }
            }
        }
        match legacy_runtime.drain_host_messages() {
            Ok(messages) => {
                for (actor, payload) in messages {
                    match serde_json::from_slice(&payload) {
                        Ok(HostMessage::SetResolvedLayout {
                            lease,
                            revision,
                            layout,
                            rest,
                            focus,
                        }) => layouts.push(PendingLayoutPublication {
                            actor,
                            lease,
                            revision,
                            layout,
                            rest,
                            focus,
                            transaction: None,
                        }),
                        Ok(HostMessage::SubmitSynchronizedLayout(request)) => {
                            let transaction = (
                                request.request_id,
                                request.lease,
                                request.base_layout_revision,
                                request.wait_for_panes,
                            );
                            layouts.push(PendingLayoutPublication {
                                actor,
                                lease: Some(request.lease),
                                revision: Some(request.base_layout_revision),
                                layout: request.fragment.layout,
                                rest: request.fragment.rest,
                                focus: request.focus,
                                transaction: Some(transaction),
                            });
                        }
                        Ok(HostMessage::FocusPane { pane }) => focus_requests.push(pane),
                        Ok(HostMessage::SetPointerPolicy { lease, provider }) => {
                            pointer_selections.push((actor, 1, lease, provider));
                        }
                        Ok(HostMessage::DelegateLayoutRest {
                            lease,
                            revision,
                            actor: child,
                            panes,
                        }) => {
                            delegations.push((actor, lease, revision, child, panes));
                        }
                        Ok(message) => eprintln!(
                            "ignored unsupported legacy presentation host message actor={actor} message={message:?}"
                        ),
                        Err(error) => eprintln!(
                            "rejected legacy presentation host message actor={actor} error={error} payload={}",
                            String::from_utf8_lossy(&payload)
                        ),
                    }
                }
            }
            Err(error) => eprintln!("could not drain legacy presentation host mailbox: {error:#}"),
        }
        for (sender, recipient, body) in messages {
            eprintln!(
                "presentation local actor message sender={sender} recipient={recipient} body={}",
                String::from_utf8_lossy(&body)
            );
            if legacy_runtime.actors.contains_key(&recipient) {
                if let Err(error) = legacy_runtime.send(&recipient, body) {
                    eprintln!("could not deliver legacy actor message to {recipient}: {error:#}");
                }
            } else if let Some(actor) = actors.get(&recipient) {
                if let Err(error) = actor.actor.try_send_message(sender, body) {
                    eprintln!(
                        "could not deliver local actor message recipient={recipient}: {error}"
                    );
                }
            }
        }
        if catalogue_requested {
            publish_backend_catalogue(
                &actors,
                &backend_catalogues,
                dynamic_backends.can_connect_remote,
                dynamic_backends.deployer.is_some(),
            );
        }
        for message in backend_messages {
            if backend_actor_messages.try_send(message).is_err() {
                eprintln!("backend actor-message queue is full");
            }
        }
        for (actor, actor_generation, lease, provider) in pointer_selections {
            let owns_lease = (lease.id == 1
                && root_layout_actor.as_deref() == Some(actor.as_str()))
                || rest_delegations
                    .values()
                    .any(|delegation| delegation.lease == lease && delegation.child == actor);
            if !owns_lease {
                eprintln!(
                    "rejected pointer-policy selection actor={actor} lease={} generation={}",
                    lease.id, lease.generation
                );
                continue;
            }
            let Some(path) = pointer_policy_module.as_ref() else {
                eprintln!(
                    "rejected pointer-policy selection actor={actor}: no provider module is configured"
                );
                continue;
            };
            match WasmPointerPolicy::load(path, &provider) {
                Ok(wasm) => {
                    pointer_capture = None;
                    pointer_policies.insert(
                        lease.id,
                        PointerPolicyBinding {
                            lease,
                            actor: actor.clone(),
                            actor_generation,
                            provider: provider.clone(),
                            wasm,
                        },
                    );
                    eprintln!(
                        "bound pointer-policy provider={provider} actor={actor} lease={} generation={}",
                        lease.id, lease.generation
                    );
                }
                Err(error) => eprintln!(
                    "could not bind pointer-policy provider={provider} actor={actor}: {error:#}"
                ),
            }
        }
        for pane in focus_requests {
            let changed = focused_pane != Some(pane);
            update_actor_focus(&actors, &mut focused_pane, pane);
            let _ = server_inputs.try_send(brood_protocol::InputEvent::WindowFocus { pane });
            eprintln!(
                "presentation key policy requested pane focus pane={pane} source=actor changed={changed} deferred=false"
            );
        }
        for (generation, message) in host_operations {
            if let HostMessage::FocusOrSpawnApp { app_id, .. } = &message
                && let Some(window) = available_windows
                    .iter()
                    .find(|window| window.app_id == *app_id)
            {
                update_actor_focus(&actors, &mut focused_pane, window.pane);
                let _ = server_inputs
                    .try_send(brood_protocol::InputEvent::WindowFocus { pane: window.pane });
                continue;
            }
            execute_host_operation(
                control_target.as_ref(),
                &presentation,
                &clipboard_sender,
                &remote_commands,
                &dynamic_backends,
                generation,
                message,
                focused_pane,
            );
        }
        for actor_id in layout_claims {
            root_layout_actor = Some(actor_id);
            layout_state_dirty = true;
        }
        if layout_state_dirty
            && let Some(actor) = root_layout_actor.as_ref().and_then(|id| actors.get(id))
        {
            send_layout_state(
                actor,
                &actors,
                &available_windows,
                presentation_size,
                next_layout_revision,
                committed_layout_revision,
                focused_pane,
                !rest_delegations.is_empty(),
            );
            next_layout_revision = next_layout_revision.saturating_add(1);
            layout_state_dirty = false;
        }
        for (generation, publication) in publications {
            if presentation
                .try_send(PresentationCommand::Publish {
                    actor_generation: generation,
                    publication,
                })
                .is_err()
            {
                eprintln!(
                    "GPUI actor supervisor stopped: presentation command rejected closed={} pending={}",
                    presentation.is_closed(),
                    presentation.pending_len()
                );
                return;
            }
        }
        for (parent, root_lease, revision, child, panes) in delegations {
            let parent_panes = if root_lease.id == 1 {
                available_windows
                    .iter()
                    .map(|window| window.pane)
                    .chain(actors.values().filter_map(|actor| actor.pane))
                    .collect::<BTreeSet<_>>()
            } else {
                rest_delegations
                    .get(&root_lease.id)
                    .map_or_else(BTreeSet::new, |delegation| {
                        delegation.panes.iter().copied().collect()
                    })
            };
            let requested = panes.iter().copied().collect::<BTreeSet<_>>();
            let parent_authorized = (root_layout_actor.as_deref() == Some(parent.as_str())
                && root_lease.id == 1)
                || rest_delegations
                    .values()
                    .any(|delegation| delegation.child == parent && delegation.lease == root_lease);
            if !parent_authorized
                || !legacy_runtime.layout_actor(&child)
                || child == parent
                || requested.len() != panes.len()
                || !requested.is_subset(&parent_panes)
            {
                eprintln!(
                    "rejected delegated layout parent={parent} child={child} lease={}",
                    root_lease.id
                );
                continue;
            }
            let existing = rest_delegations
                .values()
                .find(|current| current.parent == parent && current.parent_lease == root_lease)
                .map(|current| current.lease.id);
            if let Some(existing) = existing {
                let child_has_rest = rest_delegations
                    .values()
                    .any(|candidate| candidate.parent_lease.id == existing);
                let Some(current) = rest_delegations.get_mut(&existing) else {
                    continue;
                };
                if current.child == child && current.panes == panes {
                    current.revision = revision;
                    send_legacy_layout_state(
                        &legacy_runtime,
                        &child,
                        &actors,
                        &panes,
                        presentation_size,
                        current.lease,
                        revision,
                        committed_layout_revision,
                        focused_pane,
                        child_has_rest,
                    );
                    continue;
                }
                // Replacing a rest child revokes the complete descendant branch.
                let revoked = descendant_leases(existing, &rest_delegations);
                for lease in revoked {
                    rest_delegations.remove(&lease);
                }
            }
            let lease = brood_wasm::LayoutLeaseToken {
                id: next_layout_lease,
                generation: 1,
            };
            next_layout_lease = next_layout_lease.saturating_add(1);
            rest_delegations.insert(
                lease.id,
                RestDelegation {
                    parent: parent.clone(),
                    parent_lease: root_lease,
                    child: child.clone(),
                    panes: panes.clone(),
                    lease,
                    revision,
                    transform: None,
                    child_layout: None,
                },
            );
            send_legacy_layout_state(
                &legacy_runtime,
                &child,
                &actors,
                &panes,
                presentation_size,
                lease,
                revision,
                committed_layout_revision,
                focused_pane,
                false,
            );
            // A delegated actor can itself own a rest slot. Tell that actor as
            // soon as its child lease exists; merely dirtying the root only
            // republishes state to a Component Model root actor.
            if root_lease.id != 1
                && let Some(parent_delegation) = rest_delegations.get(&root_lease.id)
            {
                send_legacy_layout_state(
                    &legacy_runtime,
                    &parent,
                    &actors,
                    &parent_delegation.panes,
                    presentation_size,
                    parent_delegation.lease,
                    parent_delegation.revision,
                    committed_layout_revision,
                    focused_pane,
                    true,
                );
            }
            eprintln!(
                "granted delegated layout lease actor={child} lease={} panes={}",
                lease.id,
                panes.len()
            );
            // Republish the parent state only after the child lease exists so the root actor can
            // validate and submit synchronized fragments containing its `rest` slot.
            layout_state_dirty = true;
        }
        for publication in layouts {
            let is_root = root_layout_actor.as_deref() == Some(publication.actor.as_str())
                && publication.lease.is_none_or(|lease| lease.id == 1);
            let child_lease = rest_delegations
                .values()
                .find(|delegation| {
                    delegation.child == publication.actor
                        && publication.lease == Some(delegation.lease)
                })
                .map(|delegation| delegation.lease.id);
            let allowed_panes = if let Some(lease) = child_lease {
                rest_delegations
                    .get(&lease)
                    .map_or_else(BTreeSet::new, |delegation| {
                        delegation.panes.iter().copied().collect()
                    })
            } else {
                available_windows
                    .iter()
                    .map(|window| window.pane)
                    .chain(actors.values().filter_map(|actor| actor.pane))
                    .collect()
            };
            if let Err(error) =
                validate_layout_fragment(&publication.layout, publication.rest, &allowed_panes)
            {
                eprintln!(
                    "rejected invalid layout publication actor={}: {error:#}",
                    publication.actor
                );
                continue;
            }
            if let Some((request_id, lease, base_layout_revision, wait_for_panes)) =
                publication.transaction.as_ref()
            {
                let unique = wait_for_panes.iter().copied().collect::<BTreeSet<_>>();
                let rejection = if pending_resizes.values().any(|pending| {
                    pending.actor == publication.actor
                        && pending.epoch.lease == lease.id
                        && pending.epoch.lease_generation == lease.generation
                }) {
                    Some((
                        brood_wasm::SynchronizedLayoutRejectionReason::Busy,
                        "the submitting lease already has a pending resize".to_string(),
                    ))
                } else if *base_layout_revision != committed_layout_revision {
                    Some((
                        brood_wasm::SynchronizedLayoutRejectionReason::StaleBaseLayoutRevision,
                        format!(
                            "base layout revision {base_layout_revision} does not match presented revision {committed_layout_revision}"
                        ),
                    ))
                } else if unique.len() != wait_for_panes.len() {
                    Some((
                        brood_wasm::SynchronizedLayoutRejectionReason::DuplicateWaitPane,
                        "wait pane set contains a duplicate".to_string(),
                    ))
                } else if !unique.is_subset(&allowed_panes) {
                    Some((
                        brood_wasm::SynchronizedLayoutRejectionReason::WaitPaneOutsideSubtree,
                        "wait pane set escapes the submitting lease".to_string(),
                    ))
                } else {
                    None
                };
                if let Some((reason, message)) = rejection {
                    let notification = brood_wasm::SynchronizedLayoutNotification::Rejected(
                        brood_wasm::SynchronizedLayoutRejected {
                            request_id: *request_id,
                            reason,
                            message,
                        },
                    );
                    if let Ok(body) = serde_json::to_vec(&notification) {
                        if let Some(actor) = actors.get(&publication.actor) {
                            drop(
                                actor
                                    .actor
                                    .try_send_message("layout-host".to_string(), body),
                            );
                        } else {
                            let _ = legacy_runtime.send(&publication.actor, body);
                        }
                    }
                    continue;
                }
            }
            if is_root {
                root_layout = Some(publication.layout);
                if let Some(delegation) = rest_delegations
                    .values_mut()
                    .find(|delegation| delegation.parent_lease.id == 1)
                {
                    delegation.transform = publication.rest.map(|rest| rest.transform);
                    delegation.revision = publication.revision.unwrap_or(delegation.revision);
                }
            } else if let Some(lease) = child_lease {
                if let Some(delegation) = rest_delegations.get_mut(&lease) {
                    delegation.child_layout = Some(publication.layout);
                }
                if let Some(child) = rest_delegations
                    .values_mut()
                    .find(|delegation| delegation.parent_lease.id == lease)
                {
                    child.transform = publication.rest.map(|rest| rest.transform);
                    child.revision = publication.revision.unwrap_or(child.revision);
                }
            } else {
                eprintln!(
                    "rejected layout publication actor={} lease={:?}",
                    publication.actor, publication.lease
                );
                continue;
            }
            let Some(root) = root_layout.as_ref() else {
                continue;
            };
            if rest_delegations.values().any(|delegation| {
                delegation.transform.is_none() || delegation.child_layout.is_none()
            }) {
                // Recursive layouts promote atomically. Rendering the root fragment by itself
                // would briefly make delegated panes unsized and visible in GPUI's fallback flow.
                continue;
            }
            let candidate_layout = compose_layout(root, &rest_delegations);
            let candidate_zones = compose_zones(root, &rest_delegations);
            let synchronized = publication.transaction.is_some();
            let layout_changed =
                candidate_layout != committed_layout || candidate_zones != committed_zones;
            if !synchronized && layout_changed {
                committed_layout = candidate_layout.clone();
                committed_zones.clone_from(&candidate_zones);
                committed_layout_revision = next_layout_revision;
                let _ = presentation.try_send(PresentationCommand::CommitNormalizedGeometry {
                    revision: next_layout_revision,
                    panes: committed_layout.clone(),
                });
            }
            resize_actor_panes(
                &actors,
                &candidate_layout,
                presentation_size,
                publication
                    .transaction
                    .as_ref()
                    .map(|_| next_layout_revision),
                &server_inputs,
            );
            if !synchronized && let Some(pane) = publication.focus {
                update_actor_focus(&actors, &mut focused_pane, pane);
                let _ = server_inputs.try_send(brood_protocol::InputEvent::WindowFocus { pane });
            }
            if let Some((request_id, lease, base_layout_revision, wait_for_panes)) =
                publication.transaction
            {
                let target_rects = candidate_layout
                    .iter()
                    .map(|(pane, transform)| {
                        let x = ((transform.offset.u + 1.0) * 0.5 * f64::from(presentation_size.0))
                            .round() as i32;
                        let y = ((transform.offset.v + 1.0) * 0.5 * f64::from(presentation_size.1))
                            .round() as i32;
                        let width = (transform.size.u * 0.5 * f64::from(presentation_size.0))
                            .round() as i32;
                        let height = (transform.size.v * 0.5 * f64::from(presentation_size.1))
                            .round() as i32;
                        format!("{pane}:{x}:{y}:{width}:{height}")
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                eprintln!(
                    "accepted synchronized layout actor={} request={} target_rects=[{}]",
                    publication.actor, request_id, target_rects
                );
                let epoch = brood_wasm::ResizeEpoch {
                    id: next_layout_revision,
                    lease: lease.id,
                    lease_generation: lease.generation,
                    base_layout_revision,
                    target_layout_revision: next_layout_revision,
                };
                let accepted = brood_wasm::SynchronizedLayoutNotification::Accepted(
                    brood_wasm::SynchronizedLayoutAccepted {
                        request_id,
                        resize_epoch: epoch,
                        affected_panes: wait_for_panes.clone(),
                        wait_for_panes: wait_for_panes.clone(),
                    },
                );
                if let Ok(body) = serde_json::to_vec(&accepted) {
                    if let Some(actor) = actors.get(&publication.actor) {
                        drop(
                            actor
                                .actor
                                .try_send_message("layout-host".to_string(), body),
                        );
                    } else if let Err(error) = legacy_runtime.send(&publication.actor, body) {
                        eprintln!(
                            "could not notify delegated layout actor={}: {error:#}",
                            publication.actor
                        );
                    }
                }
                let waiting = wait_for_panes.into_iter().collect::<BTreeSet<_>>();
                // Declarative managed UI is reflowed by GPUI when the geometry
                // transaction commits; unlike retained canvas content, it has
                // no size-dependent frame to stage first. Treat those panes as
                // ready immediately and wait only for canvas/external content.
                let ready = waiting
                    .iter()
                    .copied()
                    .filter(|pane| {
                        actors
                            .values()
                            .find(|actor| actor.pane == Some(*pane))
                            .is_some_and(|actor| canvas_host.is_managed_only(actor.generation))
                    })
                    .collect();
                pending_resizes.insert(
                    epoch.id,
                    PendingResizeNotification {
                        actor: publication.actor,
                        request_id,
                        epoch,
                        waiting,
                        ready,
                        layout: candidate_layout,
                        zones: candidate_zones,
                        focus: publication.focus,
                        accepted_at: std::time::Instant::now(),
                    },
                );
            }
            next_layout_revision = next_layout_revision.saturating_add(1);
        }
        for (actor_id, request) in session_commands {
            if request.backend_id != 1 {
                if let Some(commands) = remote_commands
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&request.backend_id)
                {
                    if let Err(error) =
                        commands.try_send_session(actor_runtime::SessionCommandEnvelope {
                            actor_id,
                            request,
                        })
                    {
                        eprintln!("remote session command failed: {error:#}");
                    }
                }
                continue;
            }
            if let (Some(target), Some(actor)) = (control_target.as_ref(), actors.get(&actor_id)) {
                let completion = execute_session_command(target, request);
                drop(
                    actor
                        .actor
                        .try_send_session_event(SessionEvent::CommandCompleted(completion)),
                );
            }
        }
        for (parent_id, id, path, session, capabilities) in starts {
            if actors.contains_key(&id) {
                continue;
            }
            let pane = next_pane;
            next_pane = next_pane.saturating_add(1);
            match load_actor(
                &id,
                &path,
                Some(pane),
                session,
                false,
                &capabilities,
                canvas_host.clone(),
            )
            .await
            {
                Ok(actor) => {
                    let generation = actor.generation;
                    eprintln!(
                        "started presentation WASM actor id={id} generation={generation} pane={pane} scope={session:?}"
                    );
                    actors.insert(id.clone(), actor);
                    // A child contributes a new pane to the inventory used by
                    // the root layout actor.  Republish that inventory after
                    // every successful start; otherwise the root can remain
                    // pinned to the snapshot it received before its children
                    // existed and can never submit a valid overlay layout.
                    layout_state_dirty = true;
                    if let Some(parent) = actors.get(&parent_id) {
                        drop(
                            parent
                                .actor
                                .try_send_child_event(ComponentChildEvent::Started {
                                    id,
                                    generation,
                                }),
                        );
                    }
                }
                Err(error) => {
                    eprintln!(
                        "could not start presentation WASM actor id={id} path={}: {error:#}",
                        path.display()
                    );
                    if let Some(parent) = actors.get(&parent_id) {
                        drop(
                            parent
                                .actor
                                .try_send_child_event(ComponentChildEvent::Failed {
                                    id,
                                    message: format!("{error:#}"),
                                }),
                        );
                    }
                }
            }
        }
        for (parent_id, id) in stops {
            let stopped = actors.remove(&id);
            if let Some(stopped) = stopped.as_ref() {
                if let Some(pane) = stopped.pane {
                    let _ = presentation.try_send(PresentationCommand::Publish {
                        actor_generation: stopped.generation,
                        publication: brood_protocol::managed_ui::UiPublication::RemovePane { pane },
                    });
                    let _ =
                        presentation.try_send(PresentationCommand::RemoveCanvas { target: pane });
                    if focused_pane == Some(pane) {
                        focused_pane = None;
                        if let Some(next) = committed_layout
                            .keys()
                            .copied()
                            .find(|candidate| *candidate != pane)
                        {
                            update_actor_focus(&actors, &mut focused_pane, next);
                            let _ = server_inputs
                                .try_send(brood_protocol::InputEvent::WindowFocus { pane: next });
                        }
                    }
                    pending_actor_inputs.retain(|(candidate, _)| *candidate != pane);
                    pending_resizes.retain(|_, pending| !pending.waiting.contains(&pane));
                }
                // Remove retained packets before releasing their resources;
                // presentation commands preserve this FIFO teardown order.
                canvas_host.unregister(stopped.generation);
                pure_unsupported.remove(&stopped.generation);
                latest_pure_token.remove(&stopped.generation);
                pending_pure
                    .retain(|_, (generation, _, _, _, _, _)| *generation != stopped.generation);
                let revoked = rest_delegations
                    .values()
                    .filter(|delegation| delegation.parent == id || delegation.child == id)
                    .flat_map(|delegation| {
                        descendant_leases(delegation.lease.id, &rest_delegations)
                    })
                    .collect::<BTreeSet<_>>();
                for lease in &revoked {
                    rest_delegations.remove(lease);
                    pointer_policies.remove(lease);
                }
                if pointer_capture.is_some_and(|capture| revoked.contains(&capture.zone.lease.id)) {
                    pointer_capture = None;
                }
                if root_layout_actor.as_deref() == Some(id.as_str()) {
                    root_layout_actor = None;
                    root_layout = None;
                    committed_layout.clear();
                    committed_zones.clear();
                }
                layout_state_dirty = true;
            }
            if stopped.is_some()
                && let Some(parent) = actors.get(&parent_id)
            {
                drop(
                    parent
                        .actor
                        .try_send_child_event(ComponentChildEvent::Stopped { id }),
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

fn normalize_logical_key(key: &str) -> String {
    match key {
        "enter" => "Enter",
        "escape" => "Escape",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "space" => " ",
        other if other.len() > 1 && other.starts_with('f') => {
            return other.to_ascii_uppercase();
        }
        other => return other.to_string(),
    }
    .to_string()
}

fn presentation_actor_grants(
    package_root: &std::path::Path,
    workspace_root: &std::path::Path,
    requested: &[String],
) -> Result<(FilesystemGrant, RuntimeIoConfig)> {
    let package_cwd = requested
        .iter()
        .any(|capability| capability == "runtime:package-cwd");
    let workspace_write = requested
        .iter()
        .any(|capability| capability == "filesystem:workspace-write");
    let workspace_read = requested
        .iter()
        .any(|capability| capability == "filesystem:workspace-read");
    anyhow::ensure!(
        !(workspace_read && workspace_write),
        "presentation actor cannot request both workspace-read and workspace-write"
    );
    let mut commands = Vec::new();
    let mut allow_pty = false;
    let mut allow_loopback = false;
    for capability in requested {
        match capability.as_str() {
            "filesystem:workspace-read" | "filesystem:workspace-write" | "runtime:package-cwd" => {}
            "pty" => allow_pty = true,
            "network:loopback" => allow_loopback = true,
            _ => {
                let Some(command) = capability.strip_prefix("subprocess:") else {
                    bail!("unsupported presentation actor capability {capability:?}");
                };
                anyhow::ensure!(
                    !command.is_empty()
                        && !command.contains('/')
                        && !command.contains('\\')
                        && !command.chars().any(char::is_whitespace),
                    "invalid presentation subprocess capability {capability:?}"
                );
                anyhow::ensure!(
                    !commands.iter().any(|existing| existing == command),
                    "duplicate presentation subprocess capability {capability:?}"
                );
                commands.push(command.to_string());
            }
        }
    }
    anyhow::ensure!(
        !allow_pty || !commands.is_empty(),
        "presentation actor PTY capability requires an allowed subprocess"
    );
    let workspace_filesystem = workspace_read || workspace_write;
    Ok((
        FilesystemGrant {
            host_path: if workspace_filesystem {
                workspace_root.to_path_buf()
            } else {
                package_root.to_path_buf()
            },
            guest_path: ".".to_string(),
            writable: workspace_write,
        },
        RuntimeIoConfig {
            capabilities: RuntimeIoCapabilities {
                commands,
                allow_pty,
                allow_loopback,
            },
            working_directory: (!requested.is_empty()).then(|| {
                if package_cwd {
                    package_root.to_path_buf()
                } else {
                    workspace_root.to_path_buf()
                }
            }),
            ..RuntimeIoConfig::default()
        },
    ))
}

async fn load_actor(
    id: &str,
    path: &std::path::Path,
    pane: Option<u64>,
    session: Option<(u64, u64)>,
    actor_management: bool,
    requested_capabilities: &[String],
    canvas_host: Arc<CanvasPresentationHost>,
) -> Result<LiveActor> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve actor {}", path.display()))?;
    let package_root = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let workspace_root = std::env::current_dir().context("resolve presentation workspace")?;
    let (filesystem, runtime_io) =
        presentation_actor_grants(package_root, &workspace_root, requested_capabilities)?;
    let actor = ComponentActor::load(
        &path,
        ComponentActorConfig {
            actor_management,
            command_capacity: 4_096,
            presentation_backlog: 8_192,
            session_control: true,
            presentation: Some(canvas_host.clone()),
            filesystem: Some(filesystem),
            runtime_io,
            session_messaging: true,
            daemon_messaging: true,
            ..ComponentActorConfig::default()
        },
    )
    .await?;
    let generation = actor.generation();
    if let Some(pane) = pane {
        canvas_host.register(generation, pane);
    }
    actor
        .lifecycle(
            PluginLifecycleEvent::Start(PluginStartContext {
                instance_id: id.to_string(),
                generation,
                reason: PluginStartReason::Initial,
                backend_id: session.map(|scope| scope.0),
                session_id: session.map(|scope| scope.1),
                workspace_root: ".".to_string(),
                data: vec![PluginLifecycleEntry {
                    key: "realm".to_string(),
                    value: "client".to_string(),
                }],
            }),
            std::time::Duration::from_secs(2),
        )
        .await?;
    let capabilities = brood_protocol::managed_ui::UiCapabilities::gpui_host(Default::default());
    actor
        .try_send_message(
            "managed-ui-host".to_string(),
            serde_json::to_vec(&capabilities).context("encode managed UI capabilities")?,
        )
        .map_err(|error| anyhow::anyhow!("send managed UI capabilities: {error}"))?;
    if let Some(pane) = pane {
        actor
            .try_send_presentation_event(PresentationEvent::PaneCreated {
                pane,
                width: 1280.0,
                height: 800.0,
                scale: 1.0,
            })
            .map_err(|error| anyhow::anyhow!("send pane creation: {error}"))?;
    }
    Ok(LiveActor {
        id: id.to_string(),
        pane,
        generation,
        session,
        module_root: path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf(),
        actor,
    })
}

fn send_layout_state(
    actor: &LiveActor,
    actors: &BTreeMap<String, LiveActor>,
    available_windows: &[CatalogWireWindow],
    presentation_size: (u32, u32, u32),
    revision: u64,
    presented_layout_revision: u64,
    focused_pane: Option<u64>,
    has_rest_lease: bool,
) {
    let visible_panes = actors
        .values()
        .filter_map(|actor| actor.pane)
        .collect::<Vec<_>>();
    let message = serde_json::json!({
        "type": "layout-state",
        "revision": revision,
        "presented_layout_revision": presented_layout_revision,
        "lease": { "id": 1, "generation": 1 },
        "waitable_panes": visible_panes,
        "presented_panes": visible_panes,
        "visible_panes": visible_panes,
        "available_windows": available_windows.iter().map(|window| serde_json::json!({
            "backend": 1,
            "pane": window.pane,
            "session": window.session,
        })).chain(actors.values().filter_map(|actor| {
            let (backend, session) = actor.session?;
            Some(serde_json::json!({
                "backend": backend,
                "pane": actor.pane?,
                "session": session,
            }))
        })).collect::<Vec<_>>(),
        "focused_pane": focused_pane,
        "size": { "w": presentation_size.0, "h": presentation_size.1 },
        "rest": has_rest_lease
    });
    if let Ok(body) = serde_json::to_vec(&message) {
        drop(
            actor
                .actor
                .try_send_message("layout-host".to_string(), body),
        );
    }
}

fn send_legacy_layout_state(
    runtime: &LegacyLayoutRuntime,
    actor: &str,
    actors: &BTreeMap<String, LiveActor>,
    panes: &[u64],
    presentation_size: (u32, u32, u32),
    lease: brood_wasm::LayoutLeaseToken,
    revision: u64,
    presented_layout_revision: u64,
    focused_pane: Option<u64>,
    has_rest_lease: bool,
) {
    let pane_realms = panes
        .iter()
        .filter_map(|pane| {
            actors
                .values()
                .find(|candidate| candidate.pane == Some(*pane))
                .and_then(|candidate| candidate.session)
                .map(|(backend, _)| serde_json::json!({ "pane": pane, "backend": backend }))
        })
        .collect::<Vec<_>>();
    let message = serde_json::json!({
        "type": "layout-state",
        "revision": revision,
        "inventory_revision": revision,
        "layout_revision": revision,
        "presented_layout_revision": presented_layout_revision,
        "lease": { "id": lease.id, "generation": lease.generation },
        "waitable_panes": panes,
        "visible_panes": panes,
        "pane_realms": pane_realms,
        "focused_pane": focused_pane.filter(|pane| panes.contains(pane)),
        "size": { "w": presentation_size.0, "h": presentation_size.1 },
        "rest": has_rest_lease,
    });
    if let Ok(payload) = serde_json::to_vec(&message)
        && let Err(error) = runtime.send(actor, payload)
    {
        eprintln!("could not publish delegated layout state actor={actor}: {error:#}");
    }
}

fn compose_transform(
    parent: brood_wasm::LocalTransform,
    child: brood_wasm::LocalTransform,
) -> brood_wasm::LocalTransform {
    brood_wasm::LocalTransform {
        offset: brood_wasm::LocalPoint {
            u: parent.offset.u + (child.offset.u + 1.0) * parent.size.u * 0.5,
            v: parent.offset.v + (child.offset.v + 1.0) * parent.size.v * 0.5,
        },
        size: brood_wasm::LocalPoint {
            u: child.size.u * parent.size.u * 0.5,
            v: child.size.v * parent.size.v * 0.5,
        },
    }
}

fn validate_layout_fragment(
    layout: &brood_wasm::ResolvedLayout,
    rest: Option<brood_wasm::LayoutChildSlot>,
    allowed_panes: &BTreeSet<u64>,
) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut panes = BTreeSet::new();
    for item in &layout.items {
        anyhow::ensure!(
            item.id.0 != 0 && ids.insert(item.id.0),
            "layout item identity is zero or duplicated"
        );
        validate_local_transform(item.transform)?;
        anyhow::ensure!(item.z.is_finite(), "layout item z is not finite");
        if let brood_wasm::LayoutItemKind::Pane { pane }
        | brood_wasm::LayoutItemKind::OverlayPane { pane } = item.kind
        {
            anyhow::ensure!(
                allowed_panes.contains(&pane),
                "layout names unauthorized pane {pane}"
            );
            anyhow::ensure!(panes.insert(pane), "layout repeats pane {pane}");
        }
    }
    let mut zones = BTreeSet::new();
    for zone in &layout.zones {
        anyhow::ensure!(
            zone.id.0 != 0 && zones.insert(zone.id.0),
            "interaction zone identity is zero or duplicated"
        );
        validate_local_transform(zone.transform)?;
        anyhow::ensure!(
            zone.z.is_finite() && !zone.event_mask.is_empty(),
            "interaction zone is inert or has invalid z"
        );
    }
    let mut dividers = BTreeSet::new();
    for divider in &layout.dividers {
        anyhow::ensure!(
            divider.id.0 != 0 && dividers.insert(divider.id.0),
            "divider identity is zero or duplicated"
        );
        anyhow::ensure!(
            zones.contains(&divider.zone.0),
            "divider references an unknown interaction zone"
        );
        validate_local_transform(divider.transform)?;
        anyhow::ensure!(divider.z.is_finite(), "divider z is not finite");
    }
    if let Some(rest) = rest {
        validate_local_transform(rest.transform)?;
        anyhow::ensure!(rest.z.is_finite(), "rest slot z is not finite");
    }
    Ok(())
}

fn validate_local_transform(transform: brood_wasm::LocalTransform) -> Result<()> {
    const EPSILON: f64 = 1.0e-6;
    let values = [
        transform.offset.u,
        transform.offset.v,
        transform.size.u,
        transform.size.v,
    ];
    anyhow::ensure!(
        values.into_iter().all(f64::is_finite),
        "layout transform is not finite"
    );
    anyhow::ensure!(
        transform.size.u > 0.0 && transform.size.v > 0.0,
        "layout transform is empty"
    );
    anyhow::ensure!(
        transform.offset.u >= -1.0 - EPSILON
            && transform.offset.v >= -1.0 - EPSILON
            && transform.offset.u + transform.size.u <= 1.0 + EPSILON
            && transform.offset.v + transform.size.v <= 1.0 + EPSILON,
        "layout transform exceeds its lease viewport"
    );
    Ok(())
}

fn compose_layout(
    root: &brood_wasm::ResolvedLayout,
    delegations: &BTreeMap<u64, RestDelegation>,
) -> BTreeMap<u64, brood_wasm::LocalTransform> {
    let root_lease = brood_wasm::LayoutLeaseToken {
        id: 1,
        generation: 1,
    };
    let viewport = brood_wasm::LocalTransform {
        offset: brood_wasm::LocalPoint { u: -1.0, v: -1.0 },
        size: brood_wasm::LocalPoint { u: 2.0, v: 2.0 },
    };
    let mut panes = BTreeMap::new();
    compose_layout_fragment(root, root_lease, viewport, delegations, &mut panes);
    panes
}

fn compose_layout_fragment(
    layout: &brood_wasm::ResolvedLayout,
    lease: brood_wasm::LayoutLeaseToken,
    viewport: brood_wasm::LocalTransform,
    delegations: &BTreeMap<u64, RestDelegation>,
    panes: &mut BTreeMap<u64, brood_wasm::LocalTransform>,
) {
    for item in &layout.items {
        if let brood_wasm::LayoutItemKind::Pane { pane }
        | brood_wasm::LayoutItemKind::OverlayPane { pane } = item.kind
        {
            panes.insert(pane, compose_transform(viewport, item.transform));
        }
    }
    if let Some(delegation) = delegations
        .values()
        .find(|candidate| candidate.parent_lease == lease)
        && let (Some(rest), Some(child)) = (delegation.transform, &delegation.child_layout)
    {
        compose_layout_fragment(
            child,
            delegation.lease,
            compose_transform(viewport, rest),
            delegations,
            panes,
        );
    }
}

fn compose_zones(
    root: &brood_wasm::ResolvedLayout,
    delegations: &BTreeMap<u64, RestDelegation>,
) -> Vec<ComposedZone> {
    let root_lease = brood_wasm::LayoutLeaseToken {
        id: 1,
        generation: 1,
    };
    let root_viewport = brood_wasm::LocalTransform {
        offset: brood_wasm::LocalPoint { u: -1.0, v: -1.0 },
        size: brood_wasm::LocalPoint { u: 2.0, v: 2.0 },
    };
    let mut zones = Vec::new();
    compose_zone_fragment(root, root_lease, root_viewport, delegations, &mut zones);
    zones.sort_by(|left, right| right.z.total_cmp(&left.z));
    zones
}

fn compose_zone_fragment(
    layout: &brood_wasm::ResolvedLayout,
    lease: brood_wasm::LayoutLeaseToken,
    viewport: brood_wasm::LocalTransform,
    delegations: &BTreeMap<u64, RestDelegation>,
    zones: &mut Vec<ComposedZone>,
) {
    zones.extend(layout.zones.iter().map(|zone| ComposedZone {
        lease,
        id: zone.id,
        transform: compose_transform(viewport, zone.transform),
        lease_viewport: viewport,
        z: zone.z,
        event_mask: zone.event_mask,
        cursor: zone.cursor,
    }));
    if let Some(delegation) = delegations
        .values()
        .find(|candidate| candidate.parent_lease == lease)
        && let (Some(rest), Some(child)) = (delegation.transform, &delegation.child_layout)
    {
        compose_zone_fragment(
            child,
            delegation.lease,
            compose_transform(viewport, rest),
            delegations,
            zones,
        );
    }
}

fn descendant_leases(root: u64, delegations: &BTreeMap<u64, RestDelegation>) -> Vec<u64> {
    let mut result = vec![root];
    let mut cursor = 0;
    while cursor < result.len() {
        let parent = result[cursor];
        result.extend(
            delegations
                .values()
                .filter(|delegation| delegation.parent_lease.id == parent)
                .map(|delegation| delegation.lease.id),
        );
        cursor += 1;
    }
    result
}

#[derive(Clone, Copy)]
enum NativePointerInput {
    Motion {
        x: f32,
        y: f32,
        modifiers: u32,
    },
    Button {
        x: f32,
        y: f32,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    Scroll {
        x: f32,
        y: f32,
        horizontal: f32,
        vertical: f32,
        modifiers: u32,
    },
}

impl NativePointerInput {
    const fn position(self) -> (f32, f32) {
        match self {
            Self::Motion { x, y, .. } | Self::Button { x, y, .. } | Self::Scroll { x, y, .. } => {
                (x, y)
            }
        }
    }

    const fn modifiers(self) -> u32 {
        match self {
            Self::Motion { modifiers, .. }
            | Self::Button { modifiers, .. }
            | Self::Scroll { modifiers, .. } => modifiers,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "pointer routing carries the complete policy context"
)]
fn route_pointer_input(
    input: NativePointerInput,
    actors: &BTreeMap<String, LiveActor>,
    legacy_runtime: &LegacyLayoutRuntime,
    layout: &BTreeMap<u64, brood_wasm::LocalTransform>,
    zones: &[ComposedZone],
    presentation_size: (u32, u32, u32),
    policies: &mut BTreeMap<u64, PointerPolicyBinding>,
    capture: &mut Option<PointerCaptureState>,
    next_interaction: &mut u64,
    event_sequence: &mut u64,
    focused_pane: &mut Option<u64>,
    presentation: &PresentationHandle,
    current_cursor: &mut brood_wasm::CursorRole,
    canvas_host: &CanvasPresentationHost,
    server_inputs: &mpsc::SyncSender<brood_protocol::InputEvent>,
) {
    let (x, y) = input.position();
    let width = f64::from(presentation_size.0.max(1));
    let height = f64::from(presentation_size.1.max(1));
    let point = brood_wasm::LocalPoint {
        u: f64::from(x) / width * 2.0 - 1.0,
        v: f64::from(y) / height * 2.0 - 1.0,
    };
    let (phase, mask, button) = match input {
        NativePointerInput::Motion { .. } => (
            PointerPhase::Motion,
            brood_wasm::PointerEventMask::MOTION,
            None,
        ),
        NativePointerInput::Button {
            button,
            pressed: true,
            ..
        } => (
            PointerPhase::ButtonPress,
            brood_wasm::PointerEventMask::BUTTON_PRESS,
            Some(button),
        ),
        NativePointerInput::Button {
            button,
            pressed: false,
            ..
        } => (
            PointerPhase::ButtonRelease,
            brood_wasm::PointerEventMask::RELEASE,
            Some(button),
        ),
        NativePointerInput::Scroll { .. } => (
            PointerPhase::Axis,
            brood_wasm::PointerEventMask::MOTION,
            None,
        ),
    };
    let zone = capture.map(|capture| capture.zone).or_else(|| {
        zones.iter().copied().find(|zone| {
            zone.event_mask.intersects(mask) && transform_contains(zone.transform, point)
        })
    });
    let next_cursor = zone.map_or(brood_wasm::CursorRole::Default, |zone| zone.cursor);
    if *current_cursor != next_cursor {
        *current_cursor = next_cursor;
        let _ = presentation.try_send(PresentationCommand::SetCursor(next_cursor));
    }
    let mut consumed = false;
    if let Some(zone) = zone
        && let Some(binding) = policies.get_mut(&zone.lease.id)
        && binding.lease == zone.lease
    {
        *event_sequence = event_sequence.saturating_add(1).max(1);
        let lease_point = project_local(zone.lease_viewport, point);
        let interaction = capture.map(|capture| capture.interaction);
        let result = binding.wasm.handle_pointer(PointerPolicyInvocation::new(
            PointerPolicyQuery {
                interaction,
                event_sequence: *event_sequence,
                lease: zone.lease,
                zone: zone.id,
                phase,
                local_position: lease_point,
                button,
                modifiers: input.modifiers(),
                captured: interaction.is_some(),
            },
            None,
        ));
        match result {
            Ok(result) => {
                consumed = interaction.is_some() || result.delivery == PointerDelivery::Consume;
                if result.capture == PointerCapture::Acquire {
                    let id = InteractionId((*next_interaction).max(1));
                    *next_interaction = id.0.saturating_add(1);
                    *capture = Some(PointerCaptureState {
                        interaction: id,
                        zone,
                    });
                    consumed = true;
                    eprintln!(
                        "pointer-policy capture acquired interaction={} lease={} zone={}",
                        id.0, zone.lease.id, zone.id.0
                    );
                }
                for message in result.messages {
                    if message.actor != brood_wasm::POINTER_POLICY_LEASE_HOLDER
                        && message.actor != binding.actor
                    {
                        eprintln!(
                            "rejected pointer-policy cross-actor message target={}",
                            message.actor
                        );
                        continue;
                    }
                    let payload = serde_json::to_vec(&serde_json::json!({
                        "type": "layout-pointer",
                        "interaction": capture.map(|capture| capture.interaction),
                        "event_sequence": *event_sequence,
                        "lease": zone.lease,
                        "zone": zone.id.0,
                        "phase": phase,
                        "position": lease_point,
                        "button": button,
                        "modifiers": input.modifiers(),
                        "inventory_revision": 0,
                        "base_layout_revision": 0,
                        "provider_payload": message.payload,
                    }))
                    .unwrap_or_default();
                    if let Some(actor) = actors.get(&binding.actor)
                        && actor.generation == binding.actor_generation
                    {
                        drop(
                            actor
                                .actor
                                .try_send_message("pointer-policy".to_string(), payload),
                        );
                    } else if let Err(error) = legacy_runtime.send(&binding.actor, payload) {
                        eprintln!(
                            "could not deliver pointer-policy message actor={}: {error:#}",
                            binding.actor
                        );
                    }
                }
                let release = result.capture == PointerCapture::Release
                    || matches!(input, NativePointerInput::Button { pressed: false, .. });
                if release && capture.take().is_some() {
                    eprintln!(
                        "pointer-policy capture released event_sequence={}",
                        *event_sequence
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "pointer-policy evaluation failed provider={}: {error:#}",
                    binding.provider
                );
                *capture = None;
            }
        }
    }
    if consumed {
        return;
    }
    let Some((pane, transform)) = layout
        .iter()
        .rev()
        .find(|(_, transform)| transform_contains(**transform, point))
    else {
        return;
    };
    let pane_x = (point.u - transform.offset.u) * width * 0.5;
    let pane_y = (point.v - transform.offset.v) * height * 0.5;
    let position = brood_protocol::PanePosition {
        pane: *pane,
        x: pane_x,
        y: pane_y,
    };
    match input {
        NativePointerInput::Motion { .. } => {
            if canvas_host.accepts_raw_pointer(*pane) {
                send_pane_event(
                    actors,
                    *pane,
                    PresentationEvent::PointerMoved {
                        pane: *pane,
                        x: pane_x as f32,
                        y: pane_y as f32,
                    },
                );
            }
            drop(server_inputs.try_send(brood_protocol::InputEvent::PointerMotion { position }));
        }
        NativePointerInput::Button {
            button,
            pressed,
            modifiers,
            ..
        } => {
            eprintln!(
                "presentation pointer button target=Plugin {{ pane: {}, x: {:.1}, y: {:.1} }} button={button} pressed={pressed}",
                *pane, pane_x, pane_y
            );
            if canvas_host.accepts_raw_pointer(*pane) {
                send_pane_event(
                    actors,
                    *pane,
                    PresentationEvent::PointerButton {
                        pane: *pane,
                        x: pane_x as f32,
                        y: pane_y as f32,
                        button,
                        pressed,
                        modifiers,
                    },
                );
            }
            drop(
                server_inputs.try_send(brood_protocol::InputEvent::PointerButton {
                    position,
                    button,
                    pressed,
                }),
            );
            if pressed {
                update_actor_focus(actors, focused_pane, *pane);
                drop(
                    server_inputs.try_send(brood_protocol::InputEvent::WindowFocus { pane: *pane }),
                );
            }
        }
        NativePointerInput::Scroll {
            horizontal,
            vertical,
            modifiers,
            ..
        } => {
            if canvas_host.accepts_raw_pointer(*pane) {
                send_pane_event(
                    actors,
                    *pane,
                    PresentationEvent::PointerScroll {
                        pane: *pane,
                        x: pane_x as f32,
                        y: pane_y as f32,
                        horizontal,
                        vertical,
                        modifiers,
                    },
                );
            }
            drop(
                server_inputs.try_send(brood_protocol::InputEvent::PointerAxis {
                    position,
                    horizontal: f64::from(horizontal),
                    vertical: f64::from(vertical),
                }),
            );
        }
    }
}

fn transform_contains(
    transform: brood_wasm::LocalTransform,
    point: brood_wasm::LocalPoint,
) -> bool {
    point.u >= transform.offset.u
        && point.v >= transform.offset.v
        && point.u < transform.offset.u + transform.size.u
        && point.v < transform.offset.v + transform.size.v
}

fn project_local(
    transform: brood_wasm::LocalTransform,
    point: brood_wasm::LocalPoint,
) -> brood_wasm::LocalPoint {
    brood_wasm::LocalPoint {
        u: (point.u - transform.offset.u) / transform.size.u * 2.0 - 1.0,
        v: (point.v - transform.offset.v) / transform.size.v * 2.0 - 1.0,
    }
}

fn send_pane_event(actors: &BTreeMap<String, LiveActor>, pane: u64, event: PresentationEvent) {
    if let Some(actor) = actors.values().find(|actor| actor.pane == Some(pane)) {
        drop(actor.actor.try_send_presentation_event(event));
    }
}

fn resize_actor_panes(
    actors: &BTreeMap<String, LiveActor>,
    layout: &BTreeMap<u64, brood_wasm::LocalTransform>,
    presentation_size: (u32, u32, u32),
    resize_epoch: Option<u64>,
    server_inputs: &mpsc::SyncSender<brood_protocol::InputEvent>,
) {
    let scale = presentation_size.2 as f32 / 120.0;
    for actor in actors.values() {
        let Some(pane) = actor.pane else { continue };
        let Some(transform) = layout.get(&pane) else {
            // A partially composed recursive tree is not sizing authority. Keep the pane's
            // durable size until a complete accepted layout contains it.
            continue;
        };
        let width = (transform.size.u * 0.5 * f64::from(presentation_size.0)) as f32;
        let height = (transform.size.v * 0.5 * f64::from(presentation_size.1)) as f32;
        drop(
            actor
                .actor
                .try_send_presentation_event(PresentationEvent::PaneResized {
                    pane,
                    width,
                    height,
                    scale,
                    resize_epoch,
                }),
        );
    }
    let actor_panes = actors
        .values()
        .filter_map(|actor| actor.pane)
        .collect::<BTreeSet<_>>();
    for (pane, transform) in layout {
        if actor_panes.contains(pane) {
            continue;
        }
        let logical_width = (transform.size.u * 0.5 * f64::from(presentation_size.0))
            .round()
            .max(1.0) as u32;
        let logical_height = (transform.size.v * 0.5 * f64::from(presentation_size.1))
            .round()
            .max(1.0) as u32;
        let pixel_width = (f64::from(logical_width) * f64::from(presentation_size.2) / 120.0)
            .round()
            .max(1.0) as u32;
        let pixel_height = (f64::from(logical_height) * f64::from(presentation_size.2) / 120.0)
            .round()
            .max(1.0) as u32;
        let _ = server_inputs.try_send(brood_protocol::InputEvent::WindowResize {
            pane: *pane,
            logical_width,
            logical_height,
            pixel_width,
            pixel_height,
            scale_120: presentation_size.2,
            resize_epoch,
            recover_through_epoch: None,
        });
    }
}

fn update_actor_focus(actors: &BTreeMap<String, LiveActor>, focused: &mut Option<u64>, pane: u64) {
    if *focused == Some(pane) {
        return;
    }
    if actors.values().any(|actor| actor.pane == Some(pane)) {
        eprintln!("focused local plugin pane={pane}");
    }
    if let Some(previous) = focused.replace(pane) {
        send_pane_event(
            actors,
            previous,
            PresentationEvent::PaneFocused {
                pane: previous,
                focused: false,
            },
        );
    }
    send_pane_event(
        actors,
        pane,
        PresentationEvent::PaneFocused {
            pane,
            focused: true,
        },
    );
}

fn publish_backend_catalogue(
    actors: &BTreeMap<String, LiveActor>,
    catalogues: &BTreeMap<u64, ClientCatalog>,
    can_connect_remote: bool,
    can_create_container: bool,
) {
    use brood_protocol::backend_catalogue::{
        BackendCatalogueEntry, BackendCatalogueMessage, BackendSessionEntry,
    };
    let entries = catalogues
        .values()
        .map(|catalog| BackendCatalogueEntry {
            backend: brood_protocol::BackendId(catalog.sessions.backend_id),
            label: if catalog.sessions.backend_id == 1 {
                "Local".to_string()
            } else {
                format!("Remote {}", catalog.sessions.backend_id)
            },
            sessions: catalog
                .sessions
                .sessions
                .iter()
                .map(|session| BackendSessionEntry {
                    id: session.id,
                    name: session.name.clone(),
                    active: session.active,
                    windows: session.windows,
                    working_directory: session.working_directory.clone(),
                    project_id: session.project_id.clone(),
                    project_name: session.project_name.clone(),
                    branch: session.branch.clone(),
                    worktree: session.worktree,
                    git: session.git,
                    isolation: session.isolation.clone(),
                })
                .collect(),
        })
        .collect();
    let message =
        BackendCatalogueMessage::sessions(false, can_connect_remote, can_create_container, entries);
    if let (Some(actor), Ok(body)) = (actors.get("session-switcher"), serde_json::to_vec(&message))
    {
        drop(actor.actor.try_send_message("client".to_string(), body));
    }
}

fn execute_host_operation(
    target: Option<&brood_cli::control::ControlTarget>,
    presentation: &PresentationHandle,
    clipboard_sender: &mpsc::SyncSender<(u64, u64, String)>,
    remote_commands: &Arc<Mutex<BTreeMap<u64, remote::RemoteBackendCommandLane>>>,
    dynamic_backends: &DynamicBackendHandle,
    actor_generation: u64,
    message: HostMessage,
    focused_pane: Option<u64>,
) {
    match &message {
        HostMessage::SetClipboard { text } => {
            let _ = presentation.try_send(PresentationCommand::SetClipboard { text: text.clone() });
            return;
        }
        HostMessage::RequestClipboardPaste { pane } => {
            let _ = presentation.try_send(PresentationCommand::RequestClipboard {
                actor_generation,
                pane: *pane,
                response: clipboard_sender.clone(),
            });
            return;
        }
        HostMessage::ConnectRemoteBackend { address } => {
            if let Err(error) = dynamic_backends.connect(address.clone()) {
                eprintln!("dynamic remote backend request failed: {error:#}");
            }
            return;
        }
        HostMessage::DeploySshBackend { target } => {
            if let Err(error) = dynamic_backends.deploy_ssh(target.clone()) {
                eprintln!("SSH backend deployment request failed: {error:#}");
            }
            return;
        }
        HostMessage::DeployContainerBackend { image, connection } => {
            if let Err(error) = dynamic_backends.deploy_container(image.clone(), connection.clone())
            {
                eprintln!("container backend deployment request failed: {error:#}");
            }
            return;
        }
        _ => {}
    }
    let command = match message {
        HostMessage::SpawnApp { argv } | HostMessage::FocusOrSpawnApp { argv, .. } => {
            let mut command = vec!["spawn".to_string()];
            command.extend(argv);
            Some(command)
        }
        HostMessage::SwitchActiveSessionNext => {
            Some(vec!["surface-session".to_string(), "next".to_string()])
        }
        HostMessage::CreateAndAttachSession => {
            Some(vec!["surface-session".to_string(), "new".to_string()])
        }
        HostMessage::LayoutAction { action } if action == "close-active-pane" => {
            focused_pane.map(|pane| vec!["close-pane".to_string(), pane.to_string()])
        }
        _ => None,
    };
    if let Some((backend, _)) = focused_pane.and_then(remote_pane_route) {
        if let Some(command) = command {
            let description = command.join(" ");
            if let Some(lane) = remote_commands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&backend)
                && let Err(error) = lane.try_send(description, command)
            {
                eprintln!("remote host operation failed backend={backend}: {error:#}");
            }
        }
        return;
    }
    let Some(target) = target else { return };
    if let Some(command) = command
        && let Err(error) = brood_cli::control::request_internal_command(target, &command, false)
    {
        eprintln!("host operation failed: {error:#}");
    }
}

fn execute_session_command(
    target: &brood_cli::control::ControlTarget,
    request: SessionCommandRequest,
) -> SessionCommandCompletion {
    let result: Result<Option<u64>> = match request.operation {
        SessionOperation::Refresh => {
            brood_cli::control::list_surface_sessions(target).map(|_| None)
        }
        SessionOperation::Activate => request
            .session
            .context("activate requires a session")
            .and_then(|session| {
                brood_cli::control::activate_surface_session(target, session).and_then(
                    |activated| {
                        if activated {
                            Ok(Some(session))
                        } else {
                            bail!("session was not activated")
                        }
                    },
                )
            }),
        SessionOperation::Create => request
            .name
            .clone()
            .context("create requires a name")
            .and_then(|name| brood_cli::control::create_surface_session(target, name).map(Some)),
        SessionOperation::Rename => request
            .session
            .zip(request.name.clone())
            .context("rename requires a session and name")
            .and_then(|(session, name)| {
                brood_cli::control::rename_surface_session(target, session, name).and_then(
                    |renamed| {
                        if renamed {
                            Ok(Some(session))
                        } else {
                            bail!("session was not renamed")
                        }
                    },
                )
            }),
        SessionOperation::Close => request
            .session
            .context("close requires a session")
            .and_then(|session| {
                brood_cli::control::close_surface_session(target, session).and_then(|closed| {
                    if closed {
                        Ok(Some(session))
                    } else {
                        bail!("session was not closed")
                    }
                })
            }),
    };
    match result {
        Ok(session) => SessionCommandCompletion {
            backend_id: request.backend_id,
            request: request.request,
            operation: request.operation,
            success: true,
            session,
            message: "completed".to_string(),
        },
        Err(error) => SessionCommandCompletion {
            backend_id: request.backend_id,
            request: request.request,
            operation: request.operation,
            success: false,
            session: request.session,
            message: format!("{error:#}"),
        },
    }
}

#[derive(Deserialize)]
struct CatalogWireSnapshot {
    active_session: u64,
    sessions: Vec<CatalogWireSession>,
    windows: Vec<CatalogWireWindow>,
}

#[derive(Clone, Deserialize)]
struct CatalogWireWindow {
    pane: u64,
    session: u64,
    #[serde(rename = "app_id")]
    app_id: String,
    #[serde(rename = "size_authority")]
    _size_authority: brood_protocol::ManagedWindowSizeAuthority,
    #[serde(rename = "automation")]
    _automation: brood_protocol::ManagedWindowAutomationState,
}

#[derive(Deserialize)]
struct CatalogWireSession {
    id: u64,
    name: String,
    active: bool,
    windows: u64,
    working_directory: Option<String>,
    project_id: String,
    project_name: String,
    project_directory: String,
    branch: Option<String>,
    worktree: bool,
    git: bool,
    #[serde(default)]
    isolation: brood_protocol::SessionIsolationStatus,
}

struct PendingCatalog {
    revision: u64,
    expected_len: usize,
    expected_chunks: usize,
    chunks: usize,
    payload: Vec<u8>,
}

fn spawn_catalog_reader(
    mut catalog: brood_ipc::CriticalConsumer,
    notifications: brood_ipc::EventNotifier,
    snapshots: mpsc::SyncSender<ClientCatalog>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-catalog".to_string())
        .spawn(move || {
            let mut pending: Option<PendingCatalog> = None;
            while notifications.wait().is_ok() {
                while let Some(record) = catalog.try_recv() {
                    match record.opcode {
                        brood_ipc::CATALOG_SNAPSHOT_BEGIN => {
                            let expected_len =
                                usize::try_from(record.arguments[0]).unwrap_or(usize::MAX);
                            let expected_chunks =
                                usize::try_from(record.arguments[1]).unwrap_or(usize::MAX);
                            if expected_chunks.saturating_add(2) > brood_ipc::CRITICAL_CAPACITY
                                || expected_len.div_ceil(brood_ipc::CATALOG_CHUNK_BYTES)
                                    != expected_chunks
                            {
                                return;
                            }
                            pending = Some(PendingCatalog {
                                revision: record.sequence,
                                expected_len,
                                expected_chunks,
                                chunks: 0,
                                payload: Vec::with_capacity(expected_len),
                            });
                        }
                        brood_ipc::CATALOG_SNAPSHOT_CHUNK => {
                            let Some(snapshot) = pending.as_mut() else {
                                continue;
                            };
                            if snapshot.revision != record.sequence {
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
                            if valid == 0
                                || valid
                                    > snapshot.expected_len.saturating_sub(snapshot.payload.len())
                            {
                                pending = None;
                                continue;
                            }
                            snapshot.payload.extend_from_slice(&bytes[..valid]);
                            snapshot.chunks = snapshot.chunks.saturating_add(1);
                        }
                        brood_ipc::CATALOG_SNAPSHOT_END => {
                            let Some(snapshot) = pending.take() else {
                                continue;
                            };
                            if snapshot.revision != record.sequence
                                || snapshot.payload.len() != snapshot.expected_len
                                || snapshot.chunks != snapshot.expected_chunks
                            {
                                continue;
                            }
                            let Ok(decoded) =
                                serde_json::from_slice::<CatalogWireSnapshot>(&snapshot.payload)
                            else {
                                return;
                            };
                            let windows = decoded.windows;
                            let sessions = decoded
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
                                    isolation: session.isolation,
                                })
                                .collect();
                            if snapshots
                                .try_send(ClientCatalog {
                                    sessions: SessionSnapshot {
                                        backend_id: 1,
                                        revision: snapshot.revision,
                                        active: decoded.active_session,
                                        sessions,
                                    },
                                    windows,
                                })
                                .is_err()
                            {}
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn GPUI catalogue reader")?;
    Ok(())
}

fn start_local_transport(
    args: &ClientArgs,
    target: &brood_cli::control::ControlTarget,
    presentation: PresentationHandle,
    completions: mpsc::Receiver<u64>,
    inputs: mpsc::Receiver<brood_protocol::InputEvent>,
    catalogs: mpsc::SyncSender<ClientCatalog>,
    backend_actor_messages: mpsc::Receiver<BackendActorMessage>,
    presentation_actor_messages: mpsc::SyncSender<BackendActorMessage>,
    resize_ready: mpsc::SyncSender<(u64, u64)>,
) -> Result<(u64, u64)> {
    let mut server = ServerLaunch::discover()?;
    server.config = Some(
        args.config
            .clone()
            .unwrap_or_else(brood_cli::global_config_path),
    );
    if let Some(executable) = args.server.clone() {
        server.executable = executable;
    }
    connect_or_spawn(target, &server)?;
    let fast_path = open_presentation_fast_path(
        target,
        &[FrameTransportKind::Dmabuf, FrameTransportKind::SharedMemory],
    )?;
    let frame_transport = fast_path.profile.frames;
    let generation = fast_path.generation;
    spawn_actor_message_writer(
        brood_ipc::ActorMessageProducer::map(fast_path.actor_message_fd.as_fd(), generation)?,
        brood_ipc::EventNotifier::from_owned_fd(fast_path.actor_message_notify_fd),
        generation,
        backend_actor_messages,
    )?;
    spawn_actor_message_reader(
        brood_ipc::ActorMessageConsumer::map(
            fast_path.presentation_message_fd.as_fd(),
            generation,
        )?,
        brood_ipc::EventNotifier::from_owned_fd(fast_path.presentation_message_notify_fd),
        presentation_actor_messages,
    )?;
    let frames = brood_ipc::CriticalConsumer::map(fast_path.frame_fd.as_fd(), generation)?;
    let frame_notifications = brood_ipc::EventNotifier::from_owned_fd(fast_path.frame_notify_fd);
    let input = brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), generation)?;
    let input_notifications = brood_ipc::EventNotifier::from_owned_fd(fast_path.input_notify_fd);
    let catalog = brood_ipc::CriticalConsumer::map(fast_path.catalog_fd.as_fd(), generation)?;
    let catalog_notifications =
        brood_ipc::EventNotifier::from_owned_fd(fast_path.catalog_notify_fd);
    spawn_catalog_reader(catalog, catalog_notifications, catalogs)?;
    let active_by_sequence = Arc::new(Mutex::new(BTreeMap::<u64, BTreeSet<u64>>::new()));
    spawn_completion_writer(
        completions,
        inputs,
        input,
        input_notifications,
        generation,
        Arc::clone(&active_by_sequence),
    )?;
    spawn_scene_reader(
        target.clone(),
        frames,
        frame_notifications,
        generation,
        presentation,
        active_by_sequence,
        frame_transport,
        resize_ready,
    )?;
    Ok((1, fast_path.active_session))
}

fn spawn_actor_message_writer(
    mut producer: brood_ipc::ActorMessageProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    messages: mpsc::Receiver<BackendActorMessage>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-actor-writer".to_string())
        .spawn(move || {
            let mut sequence = 0_u64;
            let mut deferred = std::collections::VecDeque::new();
            loop {
                while let Some(record) = deferred.pop_front() {
                    if let Err(record) = producer.try_send(record) {
                        deferred.push_front(record);
                        break;
                    }
                    let _ = notifications.notify();
                }
                match messages.recv_timeout(std::time::Duration::from_millis(4)) {
                    Ok(message) => {
                        sequence = sequence.wrapping_add(1);
                        let Ok(record) = brood_ipc::ActorMessageRecord::new(
                            generation,
                            sequence,
                            message.realm,
                            &message.sender,
                            &message.recipient,
                            &message.body,
                        ) else {
                            continue;
                        };
                        if let Err(record) = producer.try_send(record) {
                            if deferred.len() < 128 {
                                deferred.push_back(record);
                            }
                        } else {
                            let _ = notifications.notify();
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .context("spawn GPUI actor-message writer")?;
    Ok(())
}

fn spawn_actor_message_reader(
    mut consumer: brood_ipc::ActorMessageConsumer,
    notifications: brood_ipc::EventNotifier,
    messages: mpsc::SyncSender<BackendActorMessage>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-actor-reader".to_string())
        .spawn(move || {
            while notifications.wait().is_ok() {
                while let Some(record) = consumer.try_recv() {
                    let decoded = (|| {
                        Some(BackendActorMessage {
                            backend: 1,
                            sender: record.sender().ok()?.to_string(),
                            realm: record.realm().ok()?,
                            recipient: record.recipient().ok()?.to_string(),
                            body: record.body().ok()?.to_vec(),
                        })
                    })();
                    drop(record);
                    if let Some(message) = decoded
                        && messages.try_send(message).is_err()
                    {
                        eprintln!("presentation actor-message queue is full");
                    }
                }
            }
        })
        .context("spawn GPUI actor-message reader")?;
    Ok(())
}

fn actor_accepts_backend_message(
    actor_session: Option<(u64, u64)>,
    realm: BackendActorRealm,
) -> bool {
    match (actor_session, realm) {
        (None, _) | (Some(_), BackendActorRealm::Daemon) => true,
        (Some((_, actor_session)), BackendActorRealm::Session(session)) => actor_session == session,
    }
}

fn spawn_completion_writer(
    completions: mpsc::Receiver<u64>,
    inputs: mpsc::Receiver<brood_protocol::InputEvent>,
    mut input: brood_ipc::CriticalProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    active_by_sequence: Arc<Mutex<BTreeMap<u64, BTreeSet<u64>>>>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-frame-completions".to_string())
        .spawn(move || {
            let mut record_sequence = 0_u64;
            let mut previous = BTreeSet::new();
            loop {
                while let Ok(input_event) = inputs.try_recv() {
                    record_sequence = record_sequence.wrapping_add(1);
                    send_record(
                        &mut input,
                        &notifications,
                        generation,
                        record_sequence,
                        ClientMessage::Input(input_event),
                    );
                }
                let sequence = match completions.recv_timeout(std::time::Duration::from_millis(4)) {
                    Ok(sequence) => sequence,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let active = active_by_sequence
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&sequence)
                    .unwrap_or_default();
                for resource in previous.difference(&active).copied() {
                    record_sequence = record_sequence.wrapping_add(1);
                    send_record(
                        &mut input,
                        &notifications,
                        generation,
                        record_sequence,
                        ClientMessage::ReleaseResource { resource },
                    );
                }
                record_sequence = record_sequence.wrapping_add(1);
                send_record(
                    &mut input,
                    &notifications,
                    generation,
                    record_sequence,
                    ClientMessage::Lifecycle(ClientLifecycle::FramePresented { sequence }),
                );
                previous = active;
            }
        })
        .context("spawn GPUI frame-completion writer")?;
    Ok(())
}

fn send_record(
    producer: &mut brood_ipc::CriticalProducer,
    notifications: &brood_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
    message: ClientMessage,
) {
    let Ok(record) = brood_ipc::encode_client_record(message, generation, sequence) else {
        return;
    };
    if producer.try_send(record).is_ok() {
        let _ = notifications.notify();
    }
}

fn spawn_scene_reader(
    target: brood_cli::control::ControlTarget,
    mut frames: brood_ipc::CriticalConsumer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    presentation: PresentationHandle,
    active_by_sequence: Arc<Mutex<BTreeMap<u64, BTreeSet<u64>>>>,
    frame_transport: FrameTransportKind,
    resize_ready: mpsc::SyncSender<(u64, u64)>,
) -> Result<()> {
    thread::Builder::new()
        .name("brood-gpui-scene-reader".to_string())
        .spawn(move || {
            let mut pending = None;
            let mut resources = BTreeMap::new();
            let mut shared_resources = BTreeMap::new();
            loop {
                if notifications.wait().is_err() {
                    break;
                }
                while let Some(record) = frames.try_recv() {
                    match record.opcode {
                        brood_ipc::FRAME_SCENE_BEGIN => {
                            pending = Some(PendingScene {
                                sequence: record.sequence,
                                expected: usize::try_from(record.arguments[0])
                                    .unwrap_or(usize::MAX),
                                width: u32::try_from(record.arguments[1]).unwrap_or(u32::MAX),
                                height: u32::try_from(record.arguments[2]).unwrap_or(u32::MAX),
                                panes: Vec::new(),
                            })
                        }
                        brood_ipc::FRAME_WINDOW_DMABUF => {
                            let Some(scene) = pending.as_mut() else {
                                continue;
                            };
                            let Ok(frame) = brood_ipc::decode_window_frame_record(&record) else {
                                return;
                            };
                            if resources
                                .get(&frame.resource)
                                .is_none_or(|(source, _)| *source != frame.source)
                            {
                                let Ok(resource) =
                                    dmabuf_resource(&target, frame.resource, generation)
                                else {
                                    return;
                                };
                                let descriptor = LinuxSurfaceDescriptor {
                                    fd: resource.fd,
                                    width: resource.width,
                                    height: resource.height,
                                    fourcc: resource.fourcc,
                                    modifier: resource.modifier,
                                    stride: resource.stride,
                                    offset: resource.offset,
                                    y_inverted: resource.y_inverted,
                                    source: frame.source,
                                };
                                let Ok(surface) = descriptor.import() else {
                                    return;
                                };
                                resources.insert(frame.resource, (frame.source, surface));
                            }
                            scene.panes.push((
                                frame.pane,
                                frame.resource,
                                frame.source,
                                frame.resize_epoch,
                            ));
                        }
                        brood_ipc::FRAME_WINDOW_SHM => {
                            let Some(scene) = pending.as_mut() else {
                                continue;
                            };
                            let Ok(frame) = brood_ipc::decode_window_frame_record(&record) else {
                                return;
                            };
                            if let std::collections::btree_map::Entry::Vacant(entry) =
                                shared_resources.entry(frame.resource)
                            {
                                let Ok(resource) =
                                    shared_memory_resource(&target, frame.resource, generation)
                                else {
                                    return;
                                };
                                entry.insert(resource.image);
                            }
                            scene.panes.push((
                                frame.pane,
                                frame.resource,
                                frame.source,
                                frame.resize_epoch,
                            ));
                        }
                        brood_ipc::FRAME_SCENE_END => {
                            let Some(scene) = pending.take() else {
                                continue;
                            };
                            if scene.sequence != record.sequence
                                || scene.panes.len() != scene.expected
                            {
                                return;
                            }
                            let count = scene.panes.len().max(1) as f32;
                            let width = scene.width as f32 / count;
                            let mut active = BTreeSet::new();
                            let pane_records = scene.panes;
                            match frame_transport {
                                FrameTransportKind::Dmabuf => {
                                    let mut panes = Vec::with_capacity(pane_records.len());
                                    let mut ready = Vec::new();
                                    for (index, (pane, resource, _, resize_epoch)) in
                                        pane_records.into_iter().enumerate()
                                    {
                                        let Some((_, surface)) = resources.get(&resource).cloned()
                                        else {
                                            return;
                                        };
                                        active.insert(resource);
                                        panes.push(ExternalPane {
                                            pane,
                                            resource,
                                            geometry: PaneGeometry {
                                                x: index as f32 * width,
                                                y: 0.0,
                                                width,
                                                height: scene.height as f32,
                                            },
                                            surface,
                                        });
                                        if let Some(epoch) = resize_epoch {
                                            ready.push((pane, epoch));
                                        }
                                    }
                                    resources.retain(|resource, _| active.contains(resource));
                                    active_by_sequence
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(scene.sequence, active);
                                    let resize_epoch =
                                        ready.first().map(|(_, epoch)| *epoch).filter(|epoch| {
                                            ready.iter().all(|(_, candidate)| candidate == epoch)
                                        });
                                    let command = match resize_epoch {
                                        Some(epoch) => PresentationCommand::StageExternalScene {
                                            epoch,
                                            sequence: scene.sequence,
                                            panes,
                                        },
                                        None => PresentationCommand::CommitExternalScene {
                                            revision: scene.sequence,
                                            sequence: scene.sequence,
                                            panes,
                                        },
                                    };
                                    if presentation.try_send(command).is_err() {
                                        return;
                                    }
                                    for readiness in ready {
                                        let _ = resize_ready.try_send(readiness);
                                    }
                                }
                                FrameTransportKind::SharedMemory => {
                                    let mut panes = Vec::with_capacity(pane_records.len());
                                    let mut ready = Vec::new();
                                    for (index, (pane, resource, source, resize_epoch)) in
                                        pane_records.into_iter().enumerate()
                                    {
                                        let Some(shared) = shared_resources.get(&resource) else {
                                            return;
                                        };
                                        let Ok(image) =
                                            copied_frame_image(shared, scene.sequence, source)
                                        else {
                                            return;
                                        };
                                        active.insert(resource);
                                        panes.push(CopiedPane {
                                            pane,
                                            resource,
                                            geometry: PaneGeometry {
                                                x: index as f32 * width,
                                                y: 0.0,
                                                width,
                                                height: scene.height as f32,
                                            },
                                            image,
                                        });
                                        if let Some(epoch) = resize_epoch {
                                            ready.push((pane, epoch));
                                        }
                                    }
                                    shared_resources
                                        .retain(|resource, _| active.contains(resource));
                                    active_by_sequence
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(scene.sequence, active);
                                    let resize_epoch =
                                        ready.first().map(|(_, epoch)| *epoch).filter(|epoch| {
                                            ready.iter().all(|(_, candidate)| candidate == epoch)
                                        });
                                    let command = match resize_epoch {
                                        Some(epoch) => PresentationCommand::StageCopiedScene {
                                            epoch,
                                            sequence: scene.sequence,
                                            panes,
                                        },
                                        None => PresentationCommand::CommitCopiedScene {
                                            revision: scene.sequence,
                                            sequence: scene.sequence,
                                            panes,
                                        },
                                    };
                                    if presentation.try_send(command).is_err() {
                                        return;
                                    }
                                    for readiness in ready {
                                        let _ = resize_ready.try_send(readiness);
                                    }
                                }
                                FrameTransportKind::Network => return,
                            }
                        }
                        brood_ipc::FRAME_CLOSE_PRESENTATION => {
                            let _ = presentation.try_send(PresentationCommand::Shutdown);
                            return;
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn GPUI scene reader")?;
    Ok(())
}

fn copied_frame_image(
    shared: &brood_ipc::SharedImageConsumer,
    sequence: u64,
    source: [i32; 4],
) -> Result<Arc<gpui::RenderImage>> {
    let metadata = shared.metadata();
    let pixels = shared.pixels(sequence)?;
    let [x, y, width, height] = source;
    anyhow::ensure!(
        x >= 0 && y >= 0 && width > 0 && height > 0,
        "invalid copied source"
    );
    let (x, y, width, height) = (x as u32, y as u32, width as u32, height as u32);
    anyhow::ensure!(
        x + width <= metadata.width && y + height <= metadata.height,
        "copied source is out of bounds"
    );
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in y..y + height {
        let start = row as usize * metadata.stride as usize + x as usize * 4;
        let bytes = &pixels[start..start + width as usize * 4];
        for pixel in bytes.chunks_exact(4) {
            match metadata.pixel_format {
                FramePixelFormat::Rgba8 => rgba.extend_from_slice(pixel),
                FramePixelFormat::Bgra8 => {
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]])
                }
            }
        }
    }
    let buffer =
        image::RgbaImage::from_raw(width, height, rgba).context("construct copied frame image")?;
    Ok(Arc::new(gpui::RenderImage::new(vec![image::Frame::new(
        buffer,
    )])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(u: f64, v: f64, width: f64, height: f64) -> brood_wasm::LocalTransform {
        brood_wasm::LocalTransform {
            offset: brood_wasm::LocalPoint { u, v },
            size: brood_wasm::LocalPoint {
                u: width,
                v: height,
            },
        }
    }

    fn pane(
        id: u64,
        pane: u64,
        transform: brood_wasm::LocalTransform,
    ) -> brood_wasm::ResolvedLayoutItem {
        brood_wasm::ResolvedLayoutItem {
            id: brood_wasm::LayoutItemId(id),
            kind: brood_wasm::LayoutItemKind::Pane { pane },
            z: 0.0,
            transform,
            input: brood_wasm::InputBehavior::Pane,
        }
    }

    #[test]
    fn recursive_layout_composes_every_delegated_depth() {
        let full = transform(-1.0, -1.0, 2.0, 2.0);
        let right = transform(0.0, -1.0, 1.0, 2.0);
        let bottom = transform(-1.0, 0.0, 2.0, 1.0);
        let root = brood_wasm::ResolvedLayout {
            items: vec![pane(1, 10, transform(-1.0, -1.0, 1.0, 2.0))],
            zones: Vec::new(),
            dividers: Vec::new(),
        };
        let child = brood_wasm::ResolvedLayout {
            items: vec![pane(2, 20, transform(-1.0, -1.0, 2.0, 1.0))],
            zones: Vec::new(),
            dividers: Vec::new(),
        };
        let grandchild = brood_wasm::ResolvedLayout {
            items: vec![pane(3, 30, full)],
            zones: Vec::new(),
            dividers: Vec::new(),
        };
        let root_lease = brood_wasm::LayoutLeaseToken {
            id: 1,
            generation: 1,
        };
        let child_lease = brood_wasm::LayoutLeaseToken {
            id: 2,
            generation: 1,
        };
        let grandchild_lease = brood_wasm::LayoutLeaseToken {
            id: 3,
            generation: 1,
        };
        let delegations = BTreeMap::from([
            (
                2,
                RestDelegation {
                    parent: "root".into(),
                    parent_lease: root_lease,
                    child: "child".into(),
                    panes: vec![20, 30],
                    lease: child_lease,
                    revision: 1,
                    transform: Some(right),
                    child_layout: Some(child),
                },
            ),
            (
                3,
                RestDelegation {
                    parent: "child".into(),
                    parent_lease: child_lease,
                    child: "grandchild".into(),
                    panes: vec![30],
                    lease: grandchild_lease,
                    revision: 1,
                    transform: Some(bottom),
                    child_layout: Some(grandchild),
                },
            ),
        ]);
        let composed = compose_layout(&root, &delegations);
        assert_eq!(composed.get(&10), Some(&transform(-1.0, -1.0, 1.0, 2.0)));
        assert_eq!(composed.get(&20), Some(&transform(0.0, -1.0, 1.0, 1.0)));
        assert_eq!(composed.get(&30), Some(&transform(0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn canvas_resources_are_actor_and_pane_scoped_and_bounded() {
        let (presentation, _commands) = presentation_channel(8);
        let (ready, _ready_events) = mpsc::sync_channel(1);
        let host = CanvasPresentationHost::new(presentation, ready);
        host.register(7, 42);
        assert_eq!(
            host.reserve_resource(7, 41, 1),
            Err(PresentationError::InvalidPane)
        );
        assert_eq!(
            host.reserve_resource(8, 42, 1),
            Err(PresentationError::ActorStopped)
        );
        assert_eq!(
            host.reserve_resource(7, 42, MAX_CANVAS_RESOURCE_BYTES_PER_ACTOR + 1),
            Err(PresentationError::LimitReached)
        );
        assert!(host.reserve_resource(7, 42, 16).is_ok());
    }

    #[test]
    fn canvas_packets_cannot_reference_another_actors_resource() {
        let (presentation, _commands) = presentation_channel(8);
        let (ready, _ready_events) = mpsc::sync_channel(1);
        let host = CanvasPresentationHost::new(presentation, ready);
        host.register(7, 42);
        let mut packet = brood_graphics::Packet::new();
        packet.push(brood_graphics::Command::DrawImage {
            image: 99,
            destination: brood_graphics::Rect::new(0.0, 0.0, 1.0, 1.0),
        });
        let encoded = packet.encode().unwrap_or_default();
        assert!(!encoded.is_empty());
        assert_eq!(
            host.finish_actor_frame(
                7,
                RenderToken(1),
                vec![ActorFrameDraw {
                    pane: 42,
                    resize_epoch: None,
                    state_watermark: None,
                    packet: encoded,
                }],
            ),
            Err(PresentationError::InvalidResource)
        );
    }

    #[test]
    fn invalid_actor_frame_is_rejected_before_any_presentation_command() {
        let (presentation, commands) = presentation_channel(8);
        let (ready, _ready_events) = mpsc::sync_channel(1);
        let host = CanvasPresentationHost::new(presentation, ready);
        host.register(7, 42);
        assert_eq!(host.render_targets(), vec![(7, 42, 1)]);

        let mut valid = brood_graphics::Packet::new();
        valid.push(brood_graphics::Command::FillRect {
            rect: brood_graphics::Rect::new(0.0, 0.0, 8.0, 8.0),
            color: brood_graphics::Color::rgba(1.0, 0.0, 0.0, 1.0),
        });
        let mut invalid = brood_graphics::Packet::new();
        invalid.push(brood_graphics::Command::DrawImage {
            image: 99,
            destination: brood_graphics::Rect::new(0.0, 0.0, 8.0, 8.0),
        });
        let valid = valid.encode().unwrap_or_default();
        let invalid = invalid.encode().unwrap_or_default();
        assert!(!valid.is_empty());
        assert!(!invalid.is_empty());

        assert_eq!(
            host.finish_actor_frame(
                7,
                RenderToken(1),
                vec![
                    ActorFrameDraw {
                        pane: 42,
                        resize_epoch: None,
                        state_watermark: None,
                        packet: valid,
                    },
                    ActorFrameDraw {
                        pane: 42,
                        resize_epoch: None,
                        state_watermark: None,
                        packet: invalid,
                    },
                ],
            ),
            Err(PresentationError::DuplicatePane)
        );
        assert!(commands.try_recv().is_err());
        assert_eq!(host.canvas_revision(7), None);
        assert_eq!(host.render_targets(), Vec::new());
    }
}
