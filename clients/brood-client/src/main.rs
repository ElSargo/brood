#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bounded window, text, and graphics coordinates intentionally narrow to the f32 and integer formats used by WGPU"
)]

use std::fmt::Write as _;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::{BufRead, BufReader, Cursor},
    mem::size_of,
    net::ToSocketAddrs,
    os::fd::AsFd,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use brood_cli::{
    ClientArgs,
    control::{
        ControlTarget, DmabufFrame, PresentationWindowSummary, ServerLaunch, SharedMemoryFrame,
        activate_surface_session, close_surface_session, connect_or_spawn, create_surface_session,
        dmabuf_resource, open_presentation_fast_path, rename_surface_session, request_dbus_command,
        resolve_control_target, shared_memory_resource,
    },
};
use brood_dmabuf::{SinglePlaneDmabuf, import_single_plane};
use brood_layout::{
    LayoutAction, LayoutItemKind, LayoutLeaseId, LayoutLeaseTree, LayoutRect, LayoutSize,
    LocalPoint, ResolvedLayout,
};
use brood_protocol::{
    ArtifactManifest, ArtifactRealm, BackendActorRealm, BackendId, ClientLifecycle, ClientMessage,
    FrameTransportKind, InputEvent, PanePosition,
    backend_catalogue::{
        BACKEND_CATALOGUE_ACTOR_ID, BACKEND_CATALOGUE_CLIENT_SENDER, BackendCatalogueEntry,
        BackendCatalogueMessage, BackendSessionEntry,
    },
};
use clap::Parser;
use num_traits::ToPrimitive;
use serde::Deserialize;
use wgpu::{Color, CurrentSurfaceTexture};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, ModifiersState, NamedKey},
    platform::scancode::PhysicalKeyExtScancode,
    window::{UserAttentionType, Window, WindowId},
};

mod actor_runtime;
mod backend_registry;
mod clipboard;
mod key_policy;
mod layout;
mod plugin_gpu;
mod plugin_graphics;
mod remote;

#[cfg(test)]
#[path = "../../../plugins/default-workspace/src/backend_layout.rs"]
mod default_workspace_backend_layout;

use actor_runtime::{ActorIdentity, PresentationWasmRuntime};
use backend_registry::{BackendRegistry, QualifiedPane};
use brood_wasm::{
    GlobalKeyDecision, HostMessage, PresentationEvent, SessionCommandCompletion,
    SessionCommandRequest, SessionEvent, SessionOperation, SessionSnapshot, SessionSummary,
    WasmActorEventKind, WasmPluginArtifact,
};
use brood_wayland_drop::{DropBridge as WaylandDropBridge, DropEvent as WaylandDropEvent};
use clipboard::{ClipboardBridge, ClipboardContent};
use key_policy::{ClientKeyPolicy, KeyPolicyAppPane, KeyPolicySnapshot};
use layout::{
    LayoutSource, convert_actor_layout_fragment, convert_actor_layout_lease, native_flat_layout,
};
use plugin_gpu::{GpuCanvasRenderer, GpuCanvasScene};
use plugin_graphics::PluginGraphicsBridge;

pub(crate) static PLUGIN_SCROLL_BENCH_ACTIVE: AtomicBool = AtomicBool::new(false);
const WINDOW_RESIZE_SETTLE_DELAY: Duration = Duration::from_millis(120);
const DEFAULT_WINDOW_WIDTH: f64 = 1440.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 900.0;
const MAX_AGENT_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_AGENT_IMAGE_DECODE_DIMENSION: u32 = 16_384;
const MAX_AGENT_IMAGE_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const AGENT_IMAGE_PREVIEW_WIDTH: u32 = 192;
const AGENT_IMAGE_PREVIEW_HEIGHT: u32 = 80;

const fn bound_session_for_active(
    backend: BackendId,
    active_session: u64,
) -> Option<(BackendId, u64)> {
    if active_session == 0 {
        None
    } else {
        Some((backend, active_session))
    }
}

struct AgentImagePreview {
    width: u32,
    height: u32,
    rgba8: Vec<u8>,
}

fn latency_summary(
    samples: &VecDeque<u128>,
    total_samples: u64,
    last: u128,
    maximum: u128,
) -> serde_json::Value {
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
        "key_to_present_samples": total_samples,
        "last_key_to_present_us": last,
        "mean_key_to_present_us": mean,
        "p50_key_to_present_us": percentile(50),
        "p95_key_to_present_us": percentile(95),
        "p99_key_to_present_us": percentile(99),
        "max_key_to_present_us": maximum,
        "stddev_key_to_present_us": standard_deviation,
    })
}

fn stage_summary(samples: &VecDeque<u128>) -> serde_json::Value {
    let summary = latency_summary(
        samples,
        samples.len() as u64,
        samples.back().copied().unwrap_or_default(),
        samples.iter().copied().max().unwrap_or_default(),
    );
    let field = |name| summary.get(name).cloned().unwrap_or_default();
    serde_json::json!({
        "samples": field("key_to_present_samples"),
        "last_us": field("last_key_to_present_us"),
        "mean_us": field("mean_key_to_present_us"),
        "p50_us": field("p50_key_to_present_us"),
        "p95_us": field("p95_key_to_present_us"),
        "p99_us": field("p99_key_to_present_us"),
        "max_us": field("max_key_to_present_us"),
        "stddev_us": field("stddev_key_to_present_us"),
    })
}

struct PendingPluginInput {
    pane: u64,
    backend: Option<BackendId>,
    trace_sequence: Option<u64>,
    started: Instant,
    started_unix_us: u128,
    transport_queued: Option<Instant>,
    transport_sent: Option<Instant>,
    transport_received: Option<Instant>,
    snapshot_dispatched: Option<Instant>,
    graphics_ready: Option<Instant>,
    ready: bool,
}

#[derive(Default)]
struct PluginLatencyStages {
    input_to_transport_queue: VecDeque<u128>,
    transport_send_queue: VecDeque<u128>,
    remote_round_trip: VecDeque<u128>,
    transport_round_trip: VecDeque<u128>,
    client_dispatch: VecDeque<u128>,
    plugin_render: VecDeque<u128>,
    presentation: VecDeque<u128>,
}

impl PluginLatencyStages {
    fn record(
        &mut self,
        started: Instant,
        transport_queued: Option<Instant>,
        transport_sent: Option<Instant>,
        transport_received: Instant,
        snapshot_dispatched: Instant,
        graphics_ready: Instant,
        presented: Instant,
    ) {
        if let (Some(transport_queued), Some(transport_sent)) = (transport_queued, transport_sent) {
            push_bounded(
                &mut self.input_to_transport_queue,
                transport_queued.duration_since(started).as_micros(),
            );
            push_bounded(
                &mut self.transport_send_queue,
                transport_sent.duration_since(transport_queued).as_micros(),
            );
            push_bounded(
                &mut self.remote_round_trip,
                transport_received
                    .duration_since(transport_sent)
                    .as_micros(),
            );
        }
        push_bounded(
            &mut self.transport_round_trip,
            transport_received.duration_since(started).as_micros(),
        );
        push_bounded(
            &mut self.client_dispatch,
            snapshot_dispatched
                .duration_since(transport_received)
                .as_micros(),
        );
        push_bounded(
            &mut self.plugin_render,
            graphics_ready
                .duration_since(snapshot_dispatched)
                .as_micros(),
        );
        push_bounded(
            &mut self.presentation,
            presented.duration_since(graphics_ready).as_micros(),
        );
    }
}

fn push_bounded(samples: &mut VecDeque<u128>, sample: u128) {
    samples.push_back(sample);
    if samples.len() > 4_096 {
        samples.pop_front();
    }
}

pub(crate) fn terminal_input_sequence(body: &[u8]) -> Option<u64> {
    let encoded = <[u8; 8]>::try_from(body.get(1..9)?).ok()?;
    (body.first() == Some(&b'I')).then(|| u64::from_le_bytes(encoded))
}

fn terminal_snapshot_input_sequence(body: &[u8]) -> Option<u64> {
    if body.get(..4) != Some(b"BTRM") || body.get(4) != Some(&4) {
        return None;
    }
    let encoded = <[u8; 8]>::try_from(body.get(14..22)?).ok()?;
    Some(u64::from_le_bytes(encoded))
}

fn terminal_snapshot_completes_revision(body: &[u8]) -> bool {
    let Some(chunk_index) = body
        .get(37..39)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
    else {
        return false;
    };
    let Some(chunk_count) = body
        .get(39..41)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
    else {
        return false;
    };
    chunk_count > 0 && chunk_index.saturating_add(1) == chunk_count
}

fn main() -> Result<()> {
    let args = ClientArgs::parse();
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
    let configured_key_policy = if args.wasm_key_policy.is_none() {
        brood_cli::client_key_policy_selection(&config_path)?
    } else {
        None
    };
    let configured_key_policy_spec = configured_key_policy.as_ref().map(|policy| {
        vec![
            policy.provider.clone(),
            policy.path.to_string_lossy().into_owned(),
        ]
    });
    let key_policy_spec = args
        .wasm_key_policy
        .as_deref()
        .or(configured_key_policy_spec.as_deref());
    let mut wasm_actor_specs = args.wasm_actors.clone();
    if let Some([provider, path]) = key_policy_spec
        && !wasm_actor_specs
            .iter()
            .any(|spec| spec.split_once('=').is_some_and(|(id, _)| id == provider))
        && let WasmPluginArtifact::Legacy(artifact) =
            WasmPluginArtifact::load(std::path::Path::new(path))?
        && artifact.inventory.actors.iter().any(|actor| {
            actor.name == *provider && actor.subscribes_to(WasmActorEventKind::LayoutState)
        })
    {
        wasm_actor_specs.push(format!("{provider}={path}"));
    }
    if args.check_wasm_actors {
        let mut runtime = PresentationWasmRuntime::start(
            &wasm_actor_specs,
            init_actor.as_deref(),
            Some((BackendId(1), 1)),
            None,
            None,
        )?;
        runtime
            .shutdown()
            .context("stop checked presentation WASM runtime")?;
        drop(runtime);
        eprintln!("presentation WASM actor check passed");
        return Ok(());
    }
    let mut key_policy =
        ClientKeyPolicy::load(key_policy_spec, brood_cli::global_keybindings_path())?;
    if args.check_wasm_key_policy {
        let snapshot = KeyPolicySnapshot {
            focused_pane: Some(1),
            panes: vec![1, 2],
            managed_panes: vec![2],
            app_panes: vec![KeyPolicyAppPane {
                pane: 2,
                app_id: "brood-terminal-1".to_string(),
            }],
        };
        let evaluation = key_policy.evaluate(
            44,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &snapshot,
        )?;
        if evaluation.decision != GlobalKeyDecision::Consume
            || !evaluation
                .host_messages
                .iter()
                .any(|message| matches!(message, HostMessage::FocusPane { pane: 2 }))
            || !evaluation.actor_messages.is_empty()
        {
            return Err(anyhow!(
                "presentation key policy did not resolve Ctrl+J to the next visible pane"
            ));
        }
        let terminal_snapshot = KeyPolicySnapshot {
            app_panes: vec![KeyPolicyAppPane {
                pane: 1,
                app_id: "terminal-ui".to_string(),
            }],
            ..snapshot.clone()
        };
        let terminal_control = key_policy.evaluate(
            46,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &terminal_snapshot,
        )?;
        let terminal_global = key_policy.evaluate(
            28,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            false,
            &terminal_snapshot,
        )?;
        if terminal_control.decision != GlobalKeyDecision::Abstain
            || terminal_global.decision != GlobalKeyDecision::Consume
        {
            return Err(anyhow!(
                "presentation key policy did not preserve terminal control keys and explicit global shortcuts"
            ));
        }
        let mode_toggle = key_policy.evaluate(
            56,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &snapshot,
        )?;
        let _mode_toggle_release = key_policy.evaluate(
            56,
            brood_wasm::ABI_KEY_RELEASED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &snapshot,
        )?;
        let spawn = key_policy.evaluate(
            32,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &snapshot,
        )?;
        let toggle_switcher = key_policy.evaluate(
            58,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            false,
            &snapshot,
        )?;
        let open_agent_picker = key_policy.evaluate(
            38,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            false,
            &snapshot,
        )?;
        let repeated_open_agent_picker = key_policy.evaluate(
            38,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            true,
            &snapshot,
        )?;
        let open_keybindings = key_policy.evaluate(
            brood_keybindings::KEY_SLASH,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_SHIFT,
            false,
            &snapshot,
        )?;
        let close_pane = key_policy.evaluate(
            24,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            false,
            &snapshot,
        )?;
        let repeated_toggle_switcher = key_policy.evaluate(
            58,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            true,
            &snapshot,
        )?;
        let focus_existing = key_policy.evaluate(
            10,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL,
            false,
            &snapshot,
        )?;
        if mode_toggle.decision != GlobalKeyDecision::Consume
            || spawn.decision != GlobalKeyDecision::Consume
            || !spawn.host_messages.iter().any(|message| {
                matches!(
                    message,
                    HostMessage::SpawnApp { argv }
                        if argv.first().is_some_and(|program| program == "foot")
                )
            })
            || !toggle_switcher.actor_messages.iter().any(|message| {
                message.actor == BACKEND_CATALOGUE_ACTOR_ID
                    && message.payload == br#"{"type":"sidebar-action","action":"toggle"}"#
            })
            || !open_agent_picker.actor_messages.iter().any(|message| {
                message.actor == "brood.client-init"
                    && message.payload == br#"{"kind":"coding-agent-action","action":"picker"}"#
            })
            || !open_keybindings.actor_messages.iter().any(|message| {
                message.actor == "keybindings"
                    && message
                        .payload
                        .starts_with(br#"{"type":"keybindings-show","bindings":["#)
            })
            || !repeated_open_agent_picker.actor_messages.is_empty()
            || !close_pane.host_messages.iter().any(|message| {
                matches!(
                    message,
                    HostMessage::LayoutAction { action }
                        if action == "close-active-pane"
                )
            })
            || !repeated_toggle_switcher.actor_messages.is_empty()
            || !focus_existing
                .host_messages
                .iter()
                .any(|message| matches!(message, HostMessage::FocusPane { pane: 2 }))
        {
            return Err(anyhow!(
                "presentation key policy did not preserve mode, spawn, and app-focus behavior"
            ));
        }
        eprintln!("presentation WASM key policy check passed");
        return Ok(());
    }
    let event_loop = EventLoop::<ClientEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    let event_proxy = event_loop.create_proxy();
    let plugin_graphics = PluginGraphicsBridge::new(event_proxy.clone());
    let clipboard = ClipboardBridge::start();
    let backend_deployer = backend_deployer_path();
    let mut backend_registry = BackendRegistry::default();
    let mut remote_config_template = None;
    let local_server_template = if args.no_server {
        None
    } else {
        let mut server = ServerLaunch::discover()?;
        server.config = Some(config_path.clone());
        if let Some(executable) = args.server.clone() {
            server.executable = executable;
        }
        Some(server)
    };
    let (
        input_sender,
        actor_message_sender,
        backend_commands,
        fast_path_enabled,
        backend_connected,
        bound_session_id,
        remote_connections,
    ) = if args.no_server {
        eprintln!("starting Brood presentation without a backend");
        (None, None, None, false, false, None, Vec::new())
    } else if !args.remote_connect.is_empty() {
        let mut artifact_offers = args
            .remote_offer_session_artifacts
            .iter()
            .map(|spec| brood_transport::ArtifactOffer::parse(spec, ArtifactRealm::Session))
            .collect::<Result<Vec<_>>>()?;
        for spec in &args.remote_offer_session_packages {
            artifact_offers.extend(brood_transport::package_offers(
                spec,
                ArtifactRealm::Session,
            )?);
        }
        remote_config_template = Some(remote::RemoteConfig {
            backend: BackendId(0),
            connect: args.remote_connect[0],
            server_name: args.remote_server_name.clone(),
            server_certificate: args
                .remote_server_certificate
                .clone()
                .context("--remote-server-certificate is required for remote transport")?,
            client_certificate: args
                .remote_client_certificate
                .clone()
                .context("--remote-client-certificate is required for remote transport")?,
            client_private_key: args
                .remote_client_private_key
                .clone()
                .context("--remote-client-private-key is required for remote transport")?,
            artifact_offers: artifact_offers.clone(),
            allowed_client_artifacts: args.remote_allow_client_artifacts.clone(),
            artifact_cache: args
                .remote_artifact_cache
                .clone()
                .unwrap_or_else(brood_transport::default_artifact_cache),
        });
        let mut remote_connections = Vec::new();
        let mut input_lanes = BTreeMap::new();
        let mut actor_lanes = BTreeMap::new();
        let mut command_lanes = BTreeMap::new();
        for connect in args.remote_connect {
            let backend = backend_registry.allocate(None);
            let mut connection =
                remote::RemoteConnection::start(
                    remote::RemoteConfig {
                        backend,
                        connect,
                        server_name: args.remote_server_name.clone(),
                        server_certificate: args.remote_server_certificate.clone().context(
                            "--remote-server-certificate is required for remote transport",
                        )?,
                        client_certificate: args.remote_client_certificate.clone().context(
                            "--remote-client-certificate is required for remote transport",
                        )?,
                        client_private_key: args.remote_client_private_key.clone().context(
                            "--remote-client-private-key is required for remote transport",
                        )?,
                        artifact_offers: artifact_offers.clone(),
                        allowed_client_artifacts: args.remote_allow_client_artifacts.clone(),
                        artifact_cache: args
                            .remote_artifact_cache
                            .clone()
                            .unwrap_or_else(brood_transport::default_artifact_cache),
                    },
                    &event_proxy,
                )?;
            eprintln!(
                "selected presentation backend={:?} transport carrier={:?} frames={:?} generation={} peer={connect}",
                connection.backend,
                connection.profile.carrier,
                connection.profile.frames,
                connection.generation
            );
            anyhow::ensure!(
                connection.backend == backend,
                "remote connection changed its assigned backend identity"
            );
            let (input, actor_messages, backend_commands) = connection.take_lanes()?;
            anyhow::ensure!(
                input_lanes
                    .insert(backend, Box::new(InputLane::Remote(input)))
                    .is_none(),
                "remote connections reported the duplicate backend identity {backend:?}"
            );
            actor_lanes.insert(backend, Box::new(ActorMessageLane::Remote(actor_messages)));
            command_lanes.insert(
                backend,
                Box::new(BackendCommandLane::Remote(backend_commands)),
            );
            backend_registry.observe(backend, Some(connection.generation));
            connection.activate_disconnect_notifications()?;
            remote_connections.push(connection);
        }
        let mut fast_path_enabled = false;
        let mut bound_session_id = None;
        if args.include_local {
            let target = resolve_control_target(args.dbus_name, args.dbus_address);
            let mut server = ServerLaunch::discover()?;
            server.config = Some(config_path.clone());
            if let Some(executable) = args.server.clone() {
                server.executable = executable;
            }
            let _handshake = connect_or_spawn(&target, &server)?;
            let fast_path =
                open_presentation_fast_path(&target, &args.frame_transport.capabilities())?;
            let backend = backend_registry.allocate(Some(fast_path.generation));
            let bound_session = bound_session_for_active(backend, fast_path.active_session);
            let dmabuf_import_enabled =
                fast_path.profile.frames == brood_protocol::FrameTransportKind::Dmabuf;
            let input =
                brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
            let frames =
                brood_ipc::CriticalConsumer::map(fast_path.frame_fd.as_fd(), fast_path.generation)?;
            let catalog = brood_ipc::CriticalConsumer::map(
                fast_path.catalog_fd.as_fd(),
                fast_path.generation,
            )?;
            spawn_frame_updates(
                target.clone(),
                frames,
                brood_ipc::EventNotifier::from_owned_fd(fast_path.frame_notify_fd),
                fast_path.generation,
                fast_path.profile,
                backend,
                event_loop.create_proxy(),
            )?;
            spawn_catalog_updates(
                catalog,
                brood_ipc::EventNotifier::from_owned_fd(fast_path.catalog_notify_fd),
                backend,
                event_loop.create_proxy(),
            )?;
            spawn_actor_message_updates(
                brood_ipc::ActorMessageConsumer::map(
                    fast_path.presentation_message_fd.as_fd(),
                    fast_path.generation,
                )?,
                brood_ipc::EventNotifier::from_owned_fd(fast_path.presentation_message_notify_fd),
                backend,
                event_loop.create_proxy(),
            )?;
            anyhow::ensure!(
                input_lanes
                    .insert(
                        backend,
                        Box::new(InputLane::Local(LocalInputLane {
                            producer: input,
                            notifications: brood_ipc::EventNotifier::from_owned_fd(
                                fast_path.input_notify_fd,
                            ),
                            generation: fast_path.generation,
                            sequence: 0,
                            deferred: VecDeque::new(),
                            deferred_high_water: 0,
                            coalesced_motion: 0,
                            pane_routes: BTreeMap::new(),
                        })),
                    )
                    .is_none(),
                "local backend identity collides with a remote backend: {backend:?}"
            );
            actor_lanes.insert(
                backend,
                Box::new(ActorMessageLane::Local(LocalActorMessageLane {
                    producer: brood_ipc::ActorMessageProducer::map(
                        fast_path.actor_message_fd.as_fd(),
                        fast_path.generation,
                    )?,
                    notifications: brood_ipc::EventNotifier::from_owned_fd(
                        fast_path.actor_message_notify_fd,
                    ),
                    generation: fast_path.generation,
                    sequence: 0,
                    deferred: VecDeque::new(),
                })),
            );
            command_lanes.insert(
                backend,
                Box::new(BackendCommandLane::Local(LocalBackendCommandLane::start(
                    target,
                    event_loop.create_proxy(),
                )?)),
            );
            fast_path_enabled = dmabuf_import_enabled;
            bound_session_id = bound_session;
        }
        let default_backend = bound_session_id
            .map(|(backend, _session)| backend)
            .or_else(|| input_lanes.keys().next().copied());
        (
            Some(InputLane::Multi {
                lanes: input_lanes,
                routes: BTreeMap::new(),
                default: default_backend,
            }),
            Some(ActorMessageLane::Multi {
                lanes: actor_lanes,
                default: default_backend,
            }),
            Some(BackendCommandLane::Multi {
                lanes: command_lanes,
                default: default_backend,
            }),
            fast_path_enabled,
            true,
            bound_session_id,
            remote_connections,
        )
    } else {
        let target = resolve_control_target(args.dbus_name, args.dbus_address);
        let mut server = ServerLaunch::discover()?;
        server.config = Some(config_path);
        if let Some(executable) = args.server {
            server.executable = executable;
        }
        let handshake = connect_or_spawn(&target, &server)?;
        eprintln!(
            "connected to Brood server pid={} protocol={}",
            handshake.server_pid, handshake.protocol_version
        );
        let fast_path = open_presentation_fast_path(&target, &args.frame_transport.capabilities())?;
        let backend = backend_registry.allocate(Some(fast_path.generation));
        eprintln!(
            "selected presentation transport carrier={:?} frames={:?}",
            fast_path.profile.carrier, fast_path.profile.frames
        );
        if fast_path.active_session != 0 {
            eprintln!(
                "attached presentation to active session id={}",
                fast_path.active_session
            );
        } else {
            eprintln!("attached presentation to an empty session catalogue");
        }

        let dmabuf_import_enabled =
            fast_path.profile.frames == brood_protocol::FrameTransportKind::Dmabuf;
        let (input_sender, actor_message_sender) = {
            let input =
                brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
            let frames =
                brood_ipc::CriticalConsumer::map(fast_path.frame_fd.as_fd(), fast_path.generation)?;
            let catalog = brood_ipc::CriticalConsumer::map(
                fast_path.catalog_fd.as_fd(),
                fast_path.generation,
            )?;
            let frame_notifications =
                brood_ipc::EventNotifier::from_owned_fd(fast_path.frame_notify_fd);
            let catalog_notifications =
                brood_ipc::EventNotifier::from_owned_fd(fast_path.catalog_notify_fd);
            spawn_frame_updates(
                target.clone(),
                frames,
                frame_notifications,
                fast_path.generation,
                fast_path.profile,
                backend,
                event_loop.create_proxy(),
            )?;
            spawn_catalog_updates(
                catalog,
                catalog_notifications,
                backend,
                event_loop.create_proxy(),
            )?;
            spawn_actor_message_updates(
                brood_ipc::ActorMessageConsumer::map(
                    fast_path.presentation_message_fd.as_fd(),
                    fast_path.generation,
                )?,
                brood_ipc::EventNotifier::from_owned_fd(fast_path.presentation_message_notify_fd),
                backend,
                event_loop.create_proxy(),
            )?;
            let local_input = LocalInputLane {
                producer: input,
                notifications: brood_ipc::EventNotifier::from_owned_fd(fast_path.input_notify_fd),
                generation: fast_path.generation,
                sequence: 0,
                deferred: VecDeque::new(),
                deferred_high_water: 0,
                coalesced_motion: 0,
                pane_routes: BTreeMap::new(),
            };
            let local_actor = LocalActorMessageLane {
                producer: brood_ipc::ActorMessageProducer::map(
                    fast_path.actor_message_fd.as_fd(),
                    fast_path.generation,
                )?,
                notifications: brood_ipc::EventNotifier::from_owned_fd(
                    fast_path.actor_message_notify_fd,
                ),
                generation: fast_path.generation,
                sequence: 0,
                deferred: VecDeque::new(),
            };
            (
                Some(InputLane::Multi {
                    lanes: BTreeMap::from([(backend, Box::new(InputLane::Local(local_input)))]),
                    routes: BTreeMap::new(),
                    default: Some(backend),
                }),
                Some(ActorMessageLane::Multi {
                    lanes: BTreeMap::from([(
                        backend,
                        Box::new(ActorMessageLane::Local(local_actor)),
                    )]),
                    default: Some(backend),
                }),
            )
        };
        let backend_commands = Some(BackendCommandLane::Multi {
            lanes: BTreeMap::from([(
                backend,
                Box::new(BackendCommandLane::Local(LocalBackendCommandLane::start(
                    target,
                    event_loop.create_proxy(),
                )?)),
            )]),
            default: Some(backend),
        });
        (
            input_sender,
            actor_message_sender,
            backend_commands,
            dmabuf_import_enabled,
            true,
            bound_session_for_active(backend, fast_path.active_session),
            Vec::new(),
        )
    };
    let mut wasm_runtime = PresentationWasmRuntime::start(
        &wasm_actor_specs,
        init_actor.as_deref(),
        bound_session_id,
        Some(&plugin_graphics),
        Some(event_proxy.clone()),
    )?;
    for backend in backend_registry.connected_backend_ids() {
        wasm_runtime.connect_backend(backend);
    }
    let mut application = Application {
        wasm_runtime: Some(wasm_runtime),
        key_policy: Some(key_policy),
        plugin_graphics: Some(plugin_graphics),
        clipboard,
        input_sender,
        actor_message_sender,
        backend_commands,
        fast_path_enabled,
        backend_connected,
        window_focused: true,
        event_proxy: Some(event_proxy),
        remote_config_template,
        remote_connections,
        ssh_deployer: backend_deployer.clone(),
        container_deployer: backend_deployer,
        local_server_template,
        backend_registry,
        ..Application::default()
    };
    application.publish_backend_sessions()?;

    let run_result = event_loop
        .run_app(&mut application)
        .context("run event loop");

    if application.remote_connections.is_empty()
        && let Some(sender) = application.input_sender.as_mut()
    {
        let _ = send_input(
            sender,
            ClientMessage::Lifecycle(ClientLifecycle::Disconnect),
        );
    }
    let shutdown_result = application
        .wasm_runtime
        .as_mut()
        .map_or(Ok(()), PresentationWasmRuntime::shutdown)
        .context("stop presentation WASM runtime");
    if let Some(graphics) = application.plugin_graphics.as_ref()
        && let Ok(metrics) = graphics.diagnostics()
    {
        eprintln!(
            "plugin graphics diagnostics {{\"accepted_packets\":{},\"max_packet_bytes\":{},\"max_packet_commands\":{},\"text_layouts_created\":{},\"text_layouts_dropped\":{},\"live_text_layouts\":{},\"peak_text_layouts\":{}}}",
            metrics.accepted_packets,
            metrics.max_packet_bytes,
            metrics.max_packet_commands,
            metrics.text_layouts_created,
            metrics.text_layouts_dropped,
            metrics.live_text_layouts,
            metrics.peak_text_layouts,
        );
    }
    let mut input_diagnostics = latency_summary(
        &application.plugin_key_to_present_latencies,
        application.plugin_key_to_present_samples,
        application.plugin_last_key_to_present_us,
        application.plugin_max_key_to_present_us,
    );
    let by_backend = serde_json::Value::Array(
        application
            .plugin_key_to_present_by_backend
            .iter()
            .map(|(backend, samples)| {
                let stages = application.plugin_latency_stages_by_backend.get(backend);
                serde_json::json!({
                    "backend": backend.0,
                    "latency": latency_summary(
                        samples,
                        samples.len() as u64,
                        samples.back().copied().unwrap_or_default(),
                        samples.iter().copied().max().unwrap_or_default(),
                    ),
                    "stages": {
                        "input_to_transport_queue": stages
                            .map(|stages| stage_summary(&stages.input_to_transport_queue))
                            .unwrap_or_default(),
                        "transport_send_queue": stages
                            .map(|stages| stage_summary(&stages.transport_send_queue))
                            .unwrap_or_default(),
                        "remote_round_trip": stages
                            .map(|stages| stage_summary(&stages.remote_round_trip))
                            .unwrap_or_default(),
                        "transport_round_trip": stages
                            .map(|stages| stage_summary(&stages.transport_round_trip))
                            .unwrap_or_default(),
                        "client_dispatch": stages
                            .map(|stages| stage_summary(&stages.client_dispatch))
                            .unwrap_or_default(),
                        "plugin_render": stages
                            .map(|stages| stage_summary(&stages.plugin_render))
                            .unwrap_or_default(),
                        "presentation": stages
                            .map(|stages| stage_summary(&stages.presentation))
                            .unwrap_or_default(),
                    },
                    "traces": application
                        .plugin_input_traces_by_backend
                        .get(backend)
                        .cloned()
                        .unwrap_or_default(),
                })
            })
            .collect(),
    );
    input_diagnostics
        .as_object_mut()
        .expect("latency summary must be a JSON object")
        .insert("by_backend".to_string(), by_backend);
    eprintln!("plugin input diagnostics {input_diagnostics}");
    let backend_shutdown_result = application
        .backend_commands
        .as_mut()
        .map_or(Ok(()), BackendCommandLane::shutdown)
        .context("stop presentation backend command worker");
    let remote_shutdown_result = application
        .remote_connections
        .iter_mut()
        .try_for_each(remote::RemoteConnection::shutdown)
        .context("stop remote presentation transport");
    let ssh_shutdown_result = application
        .ssh_deployments
        .iter_mut()
        .try_for_each(SshBackendDeployment::shutdown)
        .context("stop dynamically deployed SSH backends");
    let container_shutdown_result = application
        .container_deployments
        .iter_mut()
        .try_for_each(ContainerBackendDeployment::shutdown)
        .context("stop dynamically deployed container backends");
    let error = application.error.take();
    drop(application);
    run_result?;
    shutdown_result?;
    backend_shutdown_result?;
    remote_shutdown_result?;
    ssh_shutdown_result?;
    container_shutdown_result?;
    if let Some(error) = error {
        return Err(error);
    }

    Ok(())
}

#[derive(Debug)]
enum ClientEvent {
    SceneFrame {
        backend: BackendId,
        sequence: u64,
        width: u32,
        height: u32,
        windows: Vec<SceneWindowUpdate>,
    },
    FocusPresentation,
    ClosePresentation,
    TransportError(String),
    BackendDisconnected {
        backend: BackendId,
        message: String,
    },
    RemoteArtifactInstalled {
        manifest: ArtifactManifest,
        path: PathBuf,
        completion: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
    },
    PluginGraphicsChanged {
        pane: u64,
        sequence: u64,
        terminal_input_sequence: Option<u64>,
        ready_at: Instant,
    },
    RendererFailed {
        generation: u64,
        message: String,
    },
    BackendCommandCompleted {
        description: String,
        result: Result<String, String>,
    },
    BackendSnapshot {
        backend: BackendId,
        session: Option<SessionSnapshot>,
        active_session: u64,
        windows: Vec<PresentationWindowSummary>,
    },
    SessionCommandCompleted {
        actor_id: String,
        completion: SessionCommandCompletion,
    },
    SshBackendReady(SshBackendDeployment),
    SshBackendFailed(String),
    ContainerBackendReady(ContainerBackendDeployment),
    ContainerBackendFailed(String),
    RemoteBackendReady(Box<RemoteBackendReady>),
    RemoteBackendFailed {
        backend: BackendId,
        description: String,
        message: String,
    },
    RemoteTerminalInputSent {
        backend: BackendId,
        trace_sequence: u64,
        queued_at: Instant,
        sent_at: Instant,
    },
    WaylandDroppedFile(WaylandDropEvent),
    ActorHostMessagesReady,
    BackendActorMessage {
        backend: BackendId,
        realm: BackendActorRealm,
        sender: String,
        recipient: String,
        body: Vec<u8>,
        received_at: Instant,
    },
}

#[derive(Debug, Deserialize)]
struct SshAttachReady {
    connect: std::net::SocketAddr,
    server_name: String,
    server_certificate: PathBuf,
    client_certificate: PathBuf,
    client_private_key: PathBuf,
    session_package: String,
    session_root: PathBuf,
}

#[derive(Debug)]
struct SshBackendDeployment {
    target: String,
    child: Child,
    stdin: Option<ChildStdin>,
    ready: SshAttachReady,
    backend: Option<BackendId>,
}

#[derive(Debug)]
struct ContainerBackendDeployment {
    description: String,
    child: Child,
    stdin: Option<ChildStdin>,
    ready: SshAttachReady,
    backend: Option<BackendId>,
}

#[derive(Debug)]
enum RemoteAttachOrigin {
    Direct { address: String },
    Ssh(SshBackendDeployment),
    Container(ContainerBackendDeployment),
}

#[derive(Debug)]
struct RemoteBackendReady {
    connection: remote::RemoteConnection,
    origin: RemoteAttachOrigin,
}

impl RemoteAttachOrigin {
    fn description(&self) -> String {
        match self {
            Self::Direct { address } => format!("remote backend {address}"),
            Self::Ssh(deployment) => format!("SSH backend {}", deployment.target),
            Self::Container(deployment) => {
                format!("container backend {}", deployment.description)
            }
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Direct { .. } => Ok(()),
            Self::Ssh(deployment) => deployment.shutdown(),
            Self::Container(deployment) => deployment.shutdown(),
        }
    }
}

impl ContainerBackendDeployment {
    fn shutdown(&mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .with_context(|| format!("wait for container backend {}", self.description))?;
        anyhow::ensure!(
            status.success(),
            "container backend {} exited with {status}",
            self.description
        );
        Ok(())
    }
}

impl SshBackendDeployment {
    fn shutdown(&mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .with_context(|| format!("wait for SSH backend deployment {}", self.target))?;
        anyhow::ensure!(
            status.success(),
            "SSH backend deployment {} exited with {status}",
            self.target
        );
        Ok(())
    }
}

fn backend_deployer_path() -> Option<PathBuf> {
    let configured = std::env::var_os("BROOD_REMOTE_BIN")
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
        })?;
    configured.is_file().then_some(configured)
}

fn start_container_backend_deployment(
    deployer: &std::path::Path,
    image: &str,
    connection: Option<&str>,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let client = std::env::current_exe().context("resolve running brood-client executable")?;
    let description = connection.map_or_else(
        || image.to_string(),
        |connection| format!("{connection}::{image}"),
    );
    let mut command = Command::new(deployer);
    command
        .arg("attach-container")
        .arg(&image)
        .arg("--client")
        .arg(client);
    if let Some(connection) = connection {
        command.arg("--connection").arg(connection);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start container backend deployment {description}"))?;
    let stdin = child
        .stdin
        .take()
        .context("container backend deployment stdin was not captured")?;
    let stdout = child
        .stdout
        .take()
        .context("container backend deployment stdout was not captured")?;
    thread::Builder::new()
        .name("brood-client-container-deploy".to_string())
        .spawn(move || {
            let mut child = child;
            let mut stdin = Some(stdin);
            let ready = (|| -> Result<SshAttachReady> {
                let mut line = String::new();
                let bytes = BufReader::new(stdout)
                    .read_line(&mut line)
                    .context("read container backend attachment details")?;
                anyhow::ensure!(
                    bytes > 0,
                    "container deployer exited before publishing connection details"
                );
                serde_json::from_str(line.trim_end())
                    .context("decode container backend attachment details")
            })();
            match ready {
                Ok(ready) => {
                    let deployment = ContainerBackendDeployment {
                        description: description.clone(),
                        child,
                        stdin: stdin.take(),
                        ready,
                        backend: None,
                    };
                    if let Err(winit::event_loop::EventLoopClosed(event)) =
                        proxy.send_event(ClientEvent::ContainerBackendReady(deployment))
                        && let ClientEvent::ContainerBackendReady(mut deployment) = event
                    {
                        drop(deployment.stdin.take());
                        drop(deployment.child.wait());
                    }
                }
                Err(error) => {
                    drop(stdin.take());
                    let status = child.wait();
                    let message = match status {
                        Ok(status) => format!("{error:#}; deployer exited with {status}"),
                        Err(wait_error) => {
                            format!("{error:#}; could not wait for deployer: {wait_error}")
                        }
                    };
                    drop(proxy.send_event(ClientEvent::ContainerBackendFailed(message)));
                }
            }
        })
        .context("spawn container backend deployment reader")?;
    Ok(())
}

fn start_ssh_backend_deployment(
    deployer: &std::path::Path,
    target: String,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let client = std::env::current_exe().context("resolve running brood-client executable")?;
    let mut command = Command::new(deployer);
    command
        .arg("attach-ssh")
        .arg(&target)
        .arg("--client")
        .arg(client);
    if let Some(connect_host) =
        std::env::var_os("BROOD_SSH_CONNECT_HOST").filter(|host| !host.is_empty())
    {
        command.arg("--connect-host").arg(connect_host);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start SSH backend deployment for {target}"))?;
    let stdin = child
        .stdin
        .take()
        .context("SSH backend deployment stdin was not captured")?;
    let stdout = child
        .stdout
        .take()
        .context("SSH backend deployment stdout was not captured")?;
    thread::Builder::new()
        .name("brood-client-ssh-deploy".to_string())
        .spawn(move || {
            let mut child = child;
            let mut stdin = Some(stdin);
            let ready = (|| -> Result<SshAttachReady> {
                let mut line = String::new();
                let bytes = BufReader::new(stdout)
                    .read_line(&mut line)
                    .context("read SSH backend attachment details")?;
                anyhow::ensure!(
                    bytes > 0,
                    "SSH backend deployer exited before publishing connection details"
                );
                serde_json::from_str(line.trim_end())
                    .context("decode SSH backend attachment details")
            })();
            match ready {
                Ok(ready) => {
                    let deployment = SshBackendDeployment {
                        target: target.clone(),
                        child,
                        stdin: stdin.take(),
                        ready,
                        backend: None,
                    };
                    if let Err(winit::event_loop::EventLoopClosed(event)) =
                        proxy.send_event(ClientEvent::SshBackendReady(deployment))
                        && let ClientEvent::SshBackendReady(mut deployment) = event
                    {
                        drop(deployment.stdin.take());
                        drop(deployment.child.wait());
                    }
                }
                Err(error) => {
                    drop(stdin.take());
                    let status = child.wait();
                    let message = match status {
                        Ok(status) => format!("{error:#}; deployer exited with {status}"),
                        Err(wait_error) => {
                            format!("{error:#}; could not wait for deployer: {wait_error}")
                        }
                    };
                    drop(proxy.send_event(ClientEvent::SshBackendFailed(message)));
                }
            }
        })
        .context("spawn SSH backend deployment reader")?;
    Ok(())
}

#[derive(Debug)]
struct SceneWindowUpdate {
    backend: BackendId,
    window_id: u64,
    resource_id: u64,
    rect: [i32; 4],
    active: bool,
    resource: Option<FrameResource>,
}

#[derive(Debug)]
enum FrameResource {
    Dmabuf(DmabufFrame),
    SharedMemory(SharedMemoryFrame),
    Network(remote::NetworkFrame),
}

struct PendingScene {
    sequence: u64,
    expected_windows: usize,
    width: u32,
    height: u32,
    windows: Vec<SceneWindowUpdate>,
}

#[derive(Deserialize)]
struct CatalogWireSnapshot {
    active_session: u64,
    sessions: Vec<CatalogWireSession>,
    windows: Vec<CatalogWireWindow>,
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
}

#[derive(Deserialize)]
struct CatalogWireWindow {
    pane: u64,
    session: u64,
    app_id: String,
}

struct PendingCatalog {
    revision: u64,
    expected_len: usize,
    expected_chunks: usize,
    chunks: usize,
    payload: Vec<u8>,
}

fn spawn_catalog_updates(
    mut catalog: brood_ipc::CriticalConsumer,
    notifications: brood_ipc::EventNotifier,
    backend: BackendId,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let _worker = thread::Builder::new()
        .name("brood-catalog-updates".to_string())
        .spawn(move || {
            let mut pending: Option<PendingCatalog> = None;
            loop {
                if notifications.wait().is_err() {
                    break;
                }
                while let Some(record) = catalog.try_recv() {
                    match record.opcode {
                        brood_ipc::CATALOG_SNAPSHOT_BEGIN => {
                            let expected_len =
                                usize::try_from(record.arguments[0]).unwrap_or(usize::MAX);
                            let expected_chunks =
                                usize::try_from(record.arguments[1]).unwrap_or(usize::MAX);
                            let valid = expected_chunks.saturating_add(2)
                                <= brood_ipc::CRITICAL_CAPACITY
                                && expected_len.div_ceil(brood_ipc::CATALOG_CHUNK_BYTES)
                                    == expected_chunks;
                            if !valid {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "invalid catalog snapshot header revision={} bytes={} chunks={}",
                                        record.sequence, expected_len, expected_chunks
                                    )));
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
                            let remaining =
                                snapshot.expected_len.saturating_sub(snapshot.payload.len());
                            if valid == 0 || valid > remaining {
                                pending = None;
                                continue;
                            }
                            let mut bytes = [0_u8; brood_ipc::CATALOG_CHUNK_BYTES];
                            for (target, word) in bytes
                                .chunks_mut(size_of::<u64>())
                                .zip(record.arguments.iter())
                            {
                                target.copy_from_slice(&word.to_ne_bytes());
                            }
                            if let Some(chunk) = bytes.get(..valid) {
                                snapshot.payload.extend_from_slice(chunk);
                            }
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
                            let decoded =
                                match serde_json::from_slice::<CatalogWireSnapshot>(&snapshot.payload)
                                {
                                    Ok(decoded) => decoded,
                                    Err(error) => {
                                        let _send_result = proxy.send_event(
                                            ClientEvent::TransportError(format!(
                                                "decode catalog snapshot revision={}: {error}",
                                                snapshot.revision
                                            )),
                                        );
                                        return;
                                    }
                                };
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
                                })
                                .collect::<Vec<_>>();
                            let windows = decoded
                                .windows
                                .into_iter()
                                .map(|window| PresentationWindowSummary {
                                    pane: window.pane,
                                    session: window.session,
                                    app_id: window.app_id,
                                })
                                .collect::<Vec<_>>();
                            eprintln!(
                                "presentation shared catalog revision={} active={} sessions={} windows={}",
                                snapshot.revision,
                                decoded.active_session,
                                sessions.len(),
                                windows.len()
                            );
                            if proxy
                                .send_event(ClientEvent::BackendSnapshot {
                                    backend,
                                    session: Some(SessionSnapshot {
                                        backend_id: backend.0,
                                        revision: snapshot.revision,
                                        active: decoded.active_session,
                                        sessions,
                                    }),
                                    active_session: decoded.active_session,
                                    windows,
                                })
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
        .context("spawn presentation catalog update worker")?;
    Ok(())
}

fn spawn_frame_updates(
    target: ControlTarget,
    mut frames: brood_ipc::CriticalConsumer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    profile: brood_protocol::TransportProfile,
    backend: BackendId,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let _worker = thread::Builder::new()
        .name("brood-frame-updates".to_string())
        .spawn(move || {
            let mut resources = HashSet::new();
            let mut pending_scene: Option<PendingScene> = None;
            loop {
                if notifications.wait().is_err() {
                    break;
                }
                while let Some(record) = frames.try_recv() {
                    match record.opcode {
                        brood_ipc::FRAME_SCENE_BEGIN => {
                            pending_scene = Some(PendingScene {
                                sequence: record.sequence,
                                expected_windows: usize::try_from(record.arguments[0])
                                    .unwrap_or(usize::MAX),
                                width: u32::try_from(record.arguments[1]).unwrap_or(u32::MAX),
                                height: u32::try_from(record.arguments[2]).unwrap_or(u32::MAX),
                                windows: Vec::new(),
                            });
                        }
                        brood_ipc::FRAME_WINDOW_DMABUF => {
                            if profile.frames != brood_protocol::FrameTransportKind::Dmabuf {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "received DMA-BUF frame under {:?} profile",
                                        profile.frames
                                    )));
                                return;
                            }
                            let Some(scene) = pending_scene.as_mut() else {
                                continue;
                            };
                            if scene.sequence != record.sequence {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "window record sequence {} does not match scene {}",
                                        record.sequence, scene.sequence
                                    )));
                                return;
                            }
                            let resource_id = record.arguments[1];
                            let resource = if resources.insert(resource_id) {
                                match dmabuf_resource(&target, resource_id) {
                                    Ok(resource) => Some(FrameResource::Dmabuf(resource)),
                                    Err(error) => {
                                        let _send_result =
                                            proxy.send_event(ClientEvent::TransportError(format!(
                                                "fetch DMA-BUF resource {resource_id}: {error:#}"
                                            )));
                                        return;
                                    }
                                }
                            } else {
                                None
                            };
                            let (x, y) = unpack_i32_pair(record.arguments[2]);
                            let (width, height) = unpack_i32_pair(record.arguments[3]);
                            scene.windows.push(SceneWindowUpdate {
                                backend,
                                window_id: record.arguments[0],
                                resource_id,
                                rect: [x, y, width, height],
                                active: record.arguments[4] != 0,
                                resource,
                            });
                        }
                        brood_ipc::FRAME_WINDOW_SHM => {
                            if profile.frames != brood_protocol::FrameTransportKind::SharedMemory {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "received shared-memory frame under {:?} profile",
                                        profile.frames
                                    )));
                                return;
                            }
                            let Some(scene) = pending_scene.as_mut() else {
                                continue;
                            };
                            if scene.sequence != record.sequence {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "window record sequence {} does not match scene {}",
                                        record.sequence, scene.sequence
                                    )));
                                return;
                            }
                            let resource_id = record.arguments[1];
                            let resource = if resources.insert(resource_id) {
                                match shared_memory_resource(
                                    &target,
                                    resource_id,
                                    generation,
                                ) {
                                    Ok(resource) => {
                                        Some(FrameResource::SharedMemory(resource))
                                    }
                                    Err(error) => {
                                        let _send_result = proxy.send_event(
                                            ClientEvent::TransportError(format!(
                                                "fetch shared-memory resource {resource_id}: {error:#}"
                                            )),
                                        );
                                        return;
                                    }
                                }
                            } else {
                                None
                            };
                            let (x, y) = unpack_i32_pair(record.arguments[2]);
                            let (width, height) = unpack_i32_pair(record.arguments[3]);
                            scene.windows.push(SceneWindowUpdate {
                                backend,
                                window_id: record.arguments[0],
                                resource_id,
                                rect: [x, y, width, height],
                                active: record.arguments[4] != 0,
                                resource,
                            });
                        }
                        brood_ipc::FRAME_SCENE_END => {
                            let Some(scene) = pending_scene.take() else {
                                continue;
                            };
                            if scene.sequence != record.sequence
                                || scene.windows.len() != scene.expected_windows
                            {
                                let _send_result =
                                    proxy.send_event(ClientEvent::TransportError(format!(
                                        "incomplete scene {}: expected {} windows, received {}",
                                        scene.sequence,
                                        scene.expected_windows,
                                        scene.windows.len()
                                    )));
                                return;
                            }
                            if proxy
                                .send_event(ClientEvent::SceneFrame {
                                    backend,
                                    sequence: scene.sequence,
                                    width: scene.width,
                                    height: scene.height,
                                    windows: scene.windows,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        brood_ipc::FRAME_FOCUS_PRESENTATION => {
                            let _send_result = proxy.send_event(ClientEvent::FocusPresentation);
                        }
                        brood_ipc::FRAME_CLOSE_PRESENTATION => {
                            let _send_result = proxy.send_event(ClientEvent::ClosePresentation);
                        }
                        _ => {}
                    }
                }
            }
        })
        .context("spawn DMA-BUF frame update worker")?;
    Ok(())
}

fn spawn_actor_message_updates(
    mut messages: brood_ipc::ActorMessageConsumer,
    notifications: brood_ipc::EventNotifier,
    backend: BackendId,
    proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
) -> Result<()> {
    let _worker = thread::Builder::new()
        .name("brood-actor-message-updates".to_string())
        .spawn(move || {
            loop {
                if notifications.wait().is_err() {
                    return;
                }
                while let Some(message) = messages.try_recv() {
                    let decoded = (|| {
                        Ok::<_, brood_ipc::ActorMessageLaneError>(
                            ClientEvent::BackendActorMessage {
                                backend,
                                realm: message.realm()?,
                                sender: message.sender()?.to_string(),
                                recipient: message.recipient()?.to_string(),
                                body: message.body()?.to_vec(),
                                received_at: Instant::now(),
                            },
                        )
                    })();
                    drop(message);
                    match decoded {
                        Ok(event) => {
                            if proxy.send_event(event).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            if proxy
                                .send_event(ClientEvent::TransportError(format!(
                                    "decode session actor message: {error}"
                                )))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        })
        .context("spawn actor-message update worker")?;
    Ok(())
}

fn unpack_i32_pair(value: u64) -> (i32, i32) {
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
    let high = u32::try_from(value >> 32).unwrap_or_default();
    (
        i32::from_ne_bytes(low.to_ne_bytes()),
        i32::from_ne_bytes(high.to_ne_bytes()),
    )
}

enum InputLane {
    Local(LocalInputLane),
    Remote(remote::RemoteInputLane),
    Multi {
        lanes: BTreeMap<BackendId, Box<Self>>,
        routes: BTreeMap<u64, BackendId>,
        default: Option<BackendId>,
    },
}

struct LocalInputLane {
    producer: brood_ipc::CriticalProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
    deferred: VecDeque<brood_ipc::CriticalRecord>,
    deferred_high_water: usize,
    coalesced_motion: u64,
    pane_routes: BTreeMap<u64, u64>,
}

enum ActorMessageLane {
    Local(LocalActorMessageLane),
    Remote(remote::RemoteActorMessageLane),
    Multi {
        lanes: BTreeMap<BackendId, Box<Self>>,
        default: Option<BackendId>,
    },
}

struct LocalActorMessageLane {
    producer: brood_ipc::ActorMessageProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
    deferred: VecDeque<Box<brood_ipc::ActorMessageRecord>>,
}

impl LocalActorMessageLane {
    fn send(&mut self, message: &actor_runtime::BackendActorMessageRequest) -> Result<()> {
        self.flush_deferred();
        self.sequence = self.sequence.wrapping_add(1);
        let record = brood_ipc::ActorMessageRecord::new(
            self.generation,
            self.sequence,
            message.realm,
            &message.sender,
            &message.actor,
            &message.body,
        )?;
        if self.has_deferred() {
            return self.defer(record);
        }
        if let Err(record) = self.producer.try_send(record) {
            return self.defer(record);
        }
        self.notifications.notify()?;
        Ok(())
    }

    fn flush_deferred(&mut self) {
        let mut published = false;
        while let Some(record) = self.deferred.pop_front() {
            if let Err(record) = self.producer.try_send(record) {
                self.deferred.push_front(record);
                break;
            }
            published = true;
        }
        if published && let Err(error) = self.notifications.notify() {
            eprintln!("failed to notify actor-message consumer: {error}");
        }
    }

    fn defer(&mut self, record: Box<brood_ipc::ActorMessageRecord>) -> Result<()> {
        anyhow::ensure!(
            self.deferred.len() < 64,
            "presentation actor-message backlog is full"
        );
        self.deferred.push_back(record);
        Ok(())
    }

    fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }
}

impl ActorMessageLane {
    fn remove_backend(&mut self, backend: BackendId) -> bool {
        let Self::Multi { lanes, default, .. } = self else {
            return false;
        };
        let removed = lanes.remove(&backend).is_some();
        if removed && *default == Some(backend) {
            *default = lanes.keys().next().copied();
        }
        removed
    }

    fn insert_local(&mut self, backend: BackendId, lane: LocalActorMessageLane) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(lanes.insert(backend, Box::new(Self::Local(lane))).is_none());
        Ok(())
    }

    fn insert_remote(
        &mut self,
        backend: BackendId,
        lane: remote::RemoteActorMessageLane,
    ) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(
            lanes
                .insert(backend, Box::new(Self::Remote(lane)))
                .is_none(),
            "backend identity is already connected"
        );
        Ok(())
    }

    fn send(&mut self, message: &actor_runtime::BackendActorMessageRequest) -> Result<()> {
        match self {
            Self::Local(lane) => lane.send(message),
            Self::Remote(lane) => lane.send(message),
            Self::Multi { lanes, .. } => lanes
                .get_mut(&message.backend)
                .context("actor message destination backend is not connected")?
                .send(message),
        }
    }

    fn flush_deferred(&mut self) {
        match self {
            Self::Local(lane) => lane.flush_deferred(),
            Self::Remote(lane) => lane.flush_deferred(),
            Self::Multi { lanes, .. } => {
                for lane in lanes.values_mut() {
                    lane.flush_deferred();
                }
            }
        }
    }

    fn has_deferred(&self) -> bool {
        match self {
            Self::Local(lane) => lane.has_deferred(),
            Self::Remote(lane) => lane.has_deferred(),
            Self::Multi { lanes, .. } => lanes.values().any(|lane| lane.has_deferred()),
        }
    }

    const fn set_default_backend(&mut self, backend: BackendId) {
        if let Self::Multi { default, .. } = self {
            *default = Some(backend);
        }
    }
}

impl LocalInputLane {
    fn send(&mut self, message: ClientMessage) -> bool {
        self.flush_deferred();
        self.sequence = self.sequence.wrapping_add(1);
        let record = match brood_ipc::encode_client_record(message, self.generation, self.sequence)
        {
            Ok(record) => record,
            Err(error) => {
                eprintln!("cannot encode local presentation input: {error}");
                return false;
            }
        };
        if self.has_deferred() {
            self.defer(record);
            return true;
        }
        if let Err(record) = self.producer.try_send(record) {
            self.defer(record);
            return true;
        }
        if let Err(error) = self.notifications.notify() {
            eprintln!("failed to notify presentation input consumer: {error}");
        }
        true
    }

    fn flush_deferred(&mut self) {
        let had_deferred = !self.deferred.is_empty();
        let mut published = false;
        while let Some(record) = self.deferred.front().copied() {
            if self.producer.try_send(record).is_err() {
                break;
            }
            self.deferred.pop_front();
            published = true;
        }
        if published && let Err(error) = self.notifications.notify() {
            eprintln!("failed to notify presentation input consumer: {error}");
        }
        if had_deferred && self.deferred.is_empty() {
            eprintln!(
                "drained deferred presentation input records high_water={} coalesced_motion={}",
                self.deferred_high_water, self.coalesced_motion
            );
            self.deferred_high_water = 0;
            self.coalesced_motion = 0;
        }
    }

    fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    fn defer(&mut self, record: brood_ipc::CriticalRecord) {
        if brood_ipc::is_coalescible_client_record(&record)
            && let Some(previous) = self.deferred.back_mut()
            && brood_ipc::is_coalescible_client_record(previous)
        {
            *previous = record;
            self.coalesced_motion = self.coalesced_motion.saturating_add(1);
            return;
        }
        if self.deferred.is_empty() {
            eprintln!("presentation input lane backpressured; deferring input records");
        }
        self.deferred.push_back(record);
        self.deferred_high_water = self.deferred_high_water.max(self.deferred.len());
    }
}

impl InputLane {
    fn backend_count(&self) -> usize {
        match self {
            Self::Multi { lanes, .. } => lanes.len(),
            Self::Local(_) | Self::Remote(_) => 1,
        }
    }

    fn remove_backend(&mut self, backend: BackendId) -> bool {
        let Self::Multi {
            lanes,
            routes,
            default,
        } = self
        else {
            return false;
        };
        let removed = lanes.remove(&backend).is_some();
        if removed {
            routes.retain(|_pane, owner| *owner != backend);
            if *default == Some(backend) {
                *default = lanes.keys().next().copied();
            }
        }
        removed
    }

    fn insert_local(&mut self, backend: BackendId, lane: LocalInputLane) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(lanes.insert(backend, Box::new(Self::Local(lane))).is_none());
        Ok(())
    }

    fn insert_remote(&mut self, backend: BackendId, lane: remote::RemoteInputLane) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(
            lanes
                .insert(backend, Box::new(Self::Remote(lane)))
                .is_none(),
            "backend identity is already connected"
        );
        Ok(())
    }

    fn send(&mut self, message: ClientMessage) -> bool {
        let mut message = message;
        match self {
            Self::Local(lane) => {
                translate_input_panes(&mut message, &lane.pane_routes);
                lane.send(message)
            }
            Self::Remote(lane) => {
                translate_input_panes(&mut message, &lane.pane_routes());
                lane.send(message)
            }
            Self::Multi {
                lanes,
                routes,
                default,
            } => {
                if let Some(backend) = input_backend(&message, routes) {
                    return lanes
                        .get_mut(&backend)
                        .is_some_and(|lane| lane.send(message));
                }
                if let Some(lane) = default.and_then(|backend| lanes.get_mut(&backend)) {
                    return lane.send(message);
                }
                lanes
                    .values_mut()
                    .next()
                    .is_some_and(|lane| lane.send(message))
            }
        }
    }

    fn set_pane_routes(&mut self, routes: Vec<(u64, BackendId, u64)>) {
        match self {
            Self::Local(lane) => {
                lane.pane_routes = routes
                    .into_iter()
                    .map(|(view, _backend, pane)| (view, pane))
                    .collect();
            }
            Self::Remote(lane) => {
                lane.set_pane_routes(
                    routes
                        .into_iter()
                        .map(|(view, _backend, pane)| (view, pane))
                        .collect::<Vec<_>>(),
                );
            }
            Self::Multi {
                lanes,
                routes: table,
                ..
            } => {
                table.clear();
                let mut per_backend = BTreeMap::<BackendId, Vec<(u64, u64)>>::new();
                for (view, backend, pane) in routes {
                    table.insert(view, backend);
                    per_backend.entry(backend).or_default().push((view, pane));
                }
                for (backend, lane) in lanes {
                    lane.set_pane_routes(
                        per_backend
                            .remove(backend)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(view, pane)| (view, *backend, pane))
                            .collect(),
                    );
                }
            }
        }
    }

    fn flush_deferred(&mut self) {
        match self {
            Self::Local(lane) => lane.flush_deferred(),
            Self::Remote(lane) => lane.flush_deferred(),
            Self::Multi { lanes, .. } => {
                for lane in lanes.values_mut() {
                    lane.flush_deferred();
                }
            }
        }
    }

    fn has_deferred(&self) -> bool {
        match self {
            Self::Local(lane) => lane.has_deferred(),
            Self::Remote(lane) => lane.has_deferred(),
            Self::Multi { lanes, .. } => lanes.values().any(|lane| lane.has_deferred()),
        }
    }

    fn send_to(&mut self, backend: BackendId, message: ClientMessage) -> bool {
        match self {
            Self::Multi { lanes, .. } => lanes
                .get_mut(&backend)
                .is_some_and(|lane| lane.send(message)),
            Self::Local(_) | Self::Remote(_) => self.send(message),
        }
    }

    fn broadcast(&mut self, message: ClientMessage) -> bool {
        match self {
            Self::Multi { lanes, .. } => {
                let mut sent = true;
                for lane in lanes.values_mut() {
                    sent &= lane.send(message.clone());
                }
                sent
            }
            Self::Local(_) | Self::Remote(_) => self.send(message),
        }
    }

    const fn set_default_backend(&mut self, backend: BackendId) {
        if let Self::Multi { default, .. } = self {
            *default = Some(backend);
        }
    }
}

fn input_backend(message: &ClientMessage, routes: &BTreeMap<u64, BackendId>) -> Option<BackendId> {
    let ClientMessage::Input(input) = message else {
        return None;
    };
    let pane = match input {
        InputEvent::WindowFocus { pane } | InputEvent::WindowResize { pane, .. } => Some(*pane),
        InputEvent::PointerMotion { position }
        | InputEvent::PointerButton { position, .. }
        | InputEvent::PointerAxis { position, .. }
        | InputEvent::PointerWheel { position, .. } => Some(position.pane),
        _ => None,
    }?;
    routes.get(&pane).copied()
}

enum BackendCommandLane {
    Local(LocalBackendCommandLane),
    Remote(remote::RemoteBackendCommandLane),
    Multi {
        lanes: BTreeMap<BackendId, Box<Self>>,
        default: Option<BackendId>,
    },
}

struct LocalBackendCommandLane {
    sender: Option<mpsc::SyncSender<BackendRequest>>,
    worker: Option<thread::JoinHandle<()>>,
}

enum BackendRequest {
    Control(BackendCommand),
    Session(actor_runtime::SessionCommandEnvelope),
}

struct BackendCommand {
    description: String,
    argv: Vec<String>,
}

impl LocalBackendCommandLane {
    fn start(
        target: ControlTarget,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<BackendRequest>(32);
        let worker = thread::Builder::new()
            .name("brood-backend-commands".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    match request {
                        BackendRequest::Control(command) => {
                            let result = request_dbus_command(&target, &command.argv, false)
                                .map_err(|error| format!("{error:#}"));
                            if proxy
                                .send_event(ClientEvent::BackendCommandCompleted {
                                    description: command.description,
                                    result,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        BackendRequest::Session(command) => {
                            let completion = execute_session_command(&target, command.request);
                            if proxy
                                .send_event(ClientEvent::SessionCommandCompleted {
                                    actor_id: command.actor_id,
                                    completion,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            })
            .context("spawn presentation backend command worker")?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    fn try_send(&self, description: impl Into<String>, argv: Vec<String>) -> Result<()> {
        self.sender
            .as_ref()
            .context("presentation backend command worker is stopped")?
            .try_send(BackendRequest::Control(BackendCommand {
                description: description.into(),
                argv,
            }))
            .context("presentation backend command queue is full or closed")
    }

    fn try_send_session(&self, command: actor_runtime::SessionCommandEnvelope) -> Result<()> {
        self.sender
            .as_ref()
            .context("presentation backend command worker is stopped")?
            .try_send(BackendRequest::Session(command))
            .context("presentation session-command queue is full or closed")
    }

    fn shutdown(&mut self) -> Result<()> {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_panic| anyhow!("presentation backend command worker panicked"))?;
        }
        Ok(())
    }
}

impl BackendCommandLane {
    fn remove_backend(&mut self, backend: BackendId) -> Result<bool> {
        let Self::Multi { lanes, default, .. } = self else {
            return Ok(false);
        };
        let Some(mut lane) = lanes.remove(&backend) else {
            return Ok(false);
        };
        lane.shutdown()?;
        if *default == Some(backend) {
            *default = lanes.keys().next().copied();
        }
        Ok(true)
    }

    fn insert_local(&mut self, backend: BackendId, lane: LocalBackendCommandLane) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(lanes.insert(backend, Box::new(Self::Local(lane))).is_none());
        Ok(())
    }

    fn insert_remote(
        &mut self,
        backend: BackendId,
        lane: remote::RemoteBackendCommandLane,
    ) -> Result<()> {
        let Self::Multi { lanes, .. } = self else {
            anyhow::bail!("dynamic backend attachment requires multi-backend mode");
        };
        anyhow::ensure!(
            lanes
                .insert(backend, Box::new(Self::Remote(lane)))
                .is_none(),
            "backend identity is already connected"
        );
        Ok(())
    }

    fn try_send(&self, description: impl Into<String>, argv: Vec<String>) -> Result<()> {
        let description = description.into();
        match self {
            Self::Local(lane) => lane.try_send(description, argv),
            Self::Remote(lane) => lane.try_send(description, argv),
            Self::Multi { lanes, default } => default
                .and_then(|backend| lanes.get(&backend))
                .or_else(|| lanes.values().next())
                .context("no backend is available for backend command")?
                .try_send(description, argv),
        }
    }

    fn try_send_session(&self, command: actor_runtime::SessionCommandEnvelope) -> Result<()> {
        match self {
            Self::Local(lane) => lane.try_send_session(command),
            Self::Remote(lane) => lane.try_send_session(command),
            Self::Multi { lanes, default } => default
                .and_then(|backend| lanes.get(&backend))
                .or_else(|| lanes.values().next())
                .context("no backend is available for session command")?
                .try_send_session(command),
        }
    }

    fn try_send_session_to(
        &self,
        backend: BackendId,
        command: actor_runtime::SessionCommandEnvelope,
    ) -> Result<()> {
        match self {
            Self::Multi { lanes, .. } => lanes
                .get(&backend)
                .context("requested backend is not connected")?
                .try_send_session(command),
            Self::Local(_) | Self::Remote(_) => self.try_send_session(command),
        }
    }

    fn release_resource_to(&self, backend: BackendId, resource: u64) -> Result<()> {
        match self {
            Self::Local(_) => Ok(()),
            Self::Remote(lane) => lane.try_release_resource(resource),
            Self::Multi { lanes, .. } => match lanes
                .get(&backend)
                .context("resource owner backend is not connected")?
                .as_ref()
            {
                Self::Local(_) => Ok(()),
                Self::Remote(lane) => lane.try_release_resource(resource),
                Self::Multi { .. } => anyhow::bail!("nested backend command registry is invalid"),
            },
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Local(lane) => lane.shutdown(),
            Self::Remote(_) => Ok(()),
            Self::Multi { lanes, .. } => {
                for lane in lanes.values_mut() {
                    lane.shutdown()?;
                }
                Ok(())
            }
        }
    }

    const fn set_default_backend(&mut self, backend: BackendId) {
        if let Self::Multi { default, .. } = self {
            *default = Some(backend);
        }
    }
}

fn execute_session_command(
    target: &ControlTarget,
    request: SessionCommandRequest,
) -> SessionCommandCompletion {
    let request_id = request.request;
    let backend_id = request.backend_id;
    let operation = request.operation;
    let session = request.session;
    let name = request.name;
    let result = match operation {
        SessionOperation::Refresh => Ok(None),
        SessionOperation::Activate => session
            .context("activate request is missing a session ID")
            .and_then(|session| {
                activate_surface_session(target, session).and_then(|changed| {
                    anyhow::ensure!(changed, "session {session} is unavailable");
                    Ok(Some(session))
                })
            }),
        SessionOperation::Create => (|| -> Result<Option<u64>> {
            if let Some(directory) = request.directory {
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
                    .context("directory session creation returned no active session")?;
                Ok(Some(active))
            } else {
                create_surface_session(target, name.unwrap_or_default()).map(Some)
            }
        })(),
        SessionOperation::Rename => session
            .context("rename request is missing a session ID")
            .and_then(|session| {
                let name = name.context("rename request is missing a name")?;
                rename_surface_session(target, session, name).and_then(|changed| {
                    anyhow::ensure!(changed, "session {session} was not renamed");
                    Ok(Some(session))
                })
            }),
        SessionOperation::Close => session
            .context("close request is missing a session ID")
            .and_then(|session| {
                close_surface_session(target, session).and_then(|changed| {
                    anyhow::ensure!(changed, "session {session} was not closed");
                    Ok(Some(session))
                })
            }),
    };
    match result {
        Ok(session) => SessionCommandCompletion {
            backend_id,
            request: request_id,
            operation,
            success: true,
            session,
            message: String::new(),
        },
        Err(error) => SessionCommandCompletion {
            backend_id,
            request: request_id,
            operation,
            success: false,
            session,
            message: format!("{error:#}"),
        },
    }
}

#[derive(Default)]
struct PointerRoutingState {
    pending_plugin_motion: Option<(u64, f32, f32)>,
    plugin_target: Option<u64>,
    managed_target: Option<u64>,
}

struct WindowResizeTransaction {
    started: Instant,
    settle_at: Instant,
    events: u32,
    initial_size: (u32, u32),
    latest_size: (u32, u32),
}

fn drop_frontend_wayland_state<DropBridge, Frontend, Clipboard>(
    wayland_drop: &mut Option<DropBridge>,
    renderer: &mut Option<Frontend>,
    clipboard: &mut Option<Clipboard>,
) {
    // The drop bridge adopts proxies from Winit's connection, so it must go
    // before the Winit window. Both must go while the active event loop still
    // owns that connection. The clipboard uses its own Wayland connection,
    // but stopping it here keeps every frontend protocol client within the
    // same explicit shutdown boundary.
    drop(wayland_drop.take());
    drop(renderer.take());
    drop(clipboard.take());
}

impl WindowResizeTransaction {
    fn new(now: Instant, initial_size: (u32, u32), latest_size: (u32, u32)) -> Self {
        Self {
            started: now,
            settle_at: now + WINDOW_RESIZE_SETTLE_DELAY,
            events: 1,
            initial_size,
            latest_size,
        }
    }

    fn observe(&mut self, now: Instant, latest_size: (u32, u32)) {
        self.settle_at = now + WINDOW_RESIZE_SETTLE_DELAY;
        self.events = self.events.saturating_add(1);
        self.latest_size = latest_size;
    }

    fn ready(&self, now: Instant) -> bool {
        now >= self.settle_at
    }
}

#[derive(Default)]
struct Application {
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
    input_sender: Option<InputLane>,
    actor_message_sender: Option<ActorMessageLane>,
    fast_path_enabled: bool,
    backend_connected: bool,
    backend_registry: BackendRegistry,
    backend_sessions: BTreeMap<BackendId, SessionSnapshot>,
    remote_config_template: Option<remote::RemoteConfig>,
    remote_connections: Vec<remote::RemoteConnection>,
    ssh_deployer: Option<PathBuf>,
    ssh_deployments: Vec<SshBackendDeployment>,
    container_deployer: Option<PathBuf>,
    container_deployments: Vec<ContainerBackendDeployment>,
    local_server_template: Option<ServerLaunch>,
    next_dynamic_local: u64,
    pending_frames: BTreeMap<BackendId, ClientEvent>,
    pending_backend_snapshots: BTreeMap<BackendId, (u64, Vec<PresentationWindowSummary>)>,
    last_presented_sequences: BTreeMap<BackendId, u64>,
    modifiers: ModifiersState,
    window_focused: bool,
    cursor_position: Option<(f64, f64)>,
    pointer_routing: PointerRoutingState,
    pointer_grab: Option<PointerGrab>,
    wasm_runtime: Option<PresentationWasmRuntime>,
    key_policy: Option<ClientKeyPolicy>,
    plugin_graphics: Option<PluginGraphicsBridge>,
    clipboard: Option<ClipboardBridge>,
    wayland_drop: Option<WaylandDropBridge>,
    backend_commands: Option<BackendCommandLane>,
    event_proxy: Option<winit::event_loop::EventLoopProxy<ClientEvent>>,
    next_renderer_generation: u64,
    renderer_recoveries: u8,
    layout_revision: u64,
    layout_leases: LayoutLeaseTree,
    root_layout_actor: Option<ActorIdentity>,
    layout_holders: BTreeMap<LayoutLeaseId, ActorIdentity>,
    layout_publications: BTreeMap<LayoutLeaseId, u64>,
    pending_plugin_inputs: VecDeque<PendingPluginInput>,
    plugin_key_to_present_samples: u64,
    plugin_last_key_to_present_us: u128,
    plugin_max_key_to_present_us: u128,
    plugin_key_to_present_latencies: VecDeque<u128>,
    plugin_key_to_present_by_backend: BTreeMap<BackendId, VecDeque<u128>>,
    plugin_latency_stages_by_backend: BTreeMap<BackendId, PluginLatencyStages>,
    plugin_input_traces_by_backend: BTreeMap<BackendId, VecDeque<serde_json::Value>>,
    plugin_scroll_benchmark: Option<PluginScrollBenchmark>,
    plugin_scroll_auto_last_scene: Option<(u64, Instant)>,
    window_resize: Option<WindowResizeTransaction>,
}

impl Application {
    fn drop_frontend_wayland_state(&mut self) {
        drop_frontend_wayland_state(
            &mut self.wayland_drop,
            &mut self.renderer,
            &mut self.clipboard,
        );
    }

    fn disconnect_remote_backend(
        &mut self,
        backend: BackendId,
        message: &str,
    ) -> Result<Option<FocusChange>> {
        let remaining = self
            .input_sender
            .as_ref()
            .map_or(0, InputLane::backend_count);
        anyhow::ensure!(
            remaining > 1,
            "the only connected backend stopped: {message}"
        );
        if let Some(runtime) = self.wasm_runtime.as_mut() {
            runtime.disconnect_backend(backend)?;
            runtime.drain_events()?;
            runtime.discard_backend_requests(backend);
        }
        anyhow::ensure!(
            self.input_sender
                .as_mut()
                .is_some_and(|lane| lane.remove_backend(backend)),
            "disconnected backend has no input lane"
        );
        if let Some(lane) = self.actor_message_sender.as_mut() {
            let _removed = lane.remove_backend(backend);
        }
        if let Some(lane) = self.backend_commands.as_mut() {
            let _removed = lane.remove_backend(backend)?;
        }
        anyhow::ensure!(
            self.backend_registry.retire(backend),
            "disconnected backend is absent from the connection registry"
        );
        self.backend_sessions.remove(&backend);
        self.pending_frames.remove(&backend);
        self.pending_backend_snapshots.remove(&backend);
        self.last_presented_sequences.remove(&backend);
        if let Some(sender) = self.input_sender.as_mut() {
            sender.set_pane_routes(self.backend_registry.all_view_routes());
        }
        let focus_change = self
            .renderer
            .as_mut()
            .and_then(|renderer| renderer.disconnect_backend(backend));
        if let Some(position) = self
            .remote_connections
            .iter()
            .position(|connection| connection.backend == backend)
        {
            let mut connection = self.remote_connections.remove(position);
            if let Err(error) = connection.reap() {
                eprintln!("remote backend {backend:?} stopped: {error:#}");
            }
        }
        if let Some(position) = self
            .ssh_deployments
            .iter()
            .position(|deployment| deployment.backend == Some(backend))
        {
            let mut deployment = self.ssh_deployments.remove(position);
            if let Err(error) = deployment.shutdown() {
                eprintln!("SSH deployment for backend {backend:?} stopped: {error:#}");
            }
        }
        if let Some(position) = self
            .container_deployments
            .iter()
            .position(|deployment| deployment.backend == Some(backend))
        {
            let mut deployment = self.container_deployments.remove(position);
            if let Err(error) = deployment.shutdown() {
                eprintln!("container deployment for backend {backend:?} stopped: {error:#}");
            }
        }
        self.backend_connected = self
            .input_sender
            .as_ref()
            .is_some_and(|lane| lane.backend_count() != 0);
        self.publish_backend_sessions()?;
        self.notify_backend_connection_failed(message)?;
        eprintln!("detached failed remote backend={backend:?}: {message}");
        Ok(focus_change)
    }

    fn connect_local_backend(&mut self) -> Result<()> {
        let launch = self
            .local_server_template
            .clone()
            .context("dynamic local backends are unavailable without a server configuration")?;
        self.next_dynamic_local = self.next_dynamic_local.saturating_add(1).max(1);
        let destination = format!(
            "org.brood.Session.dynamic.{}.{}",
            std::process::id(),
            self.next_dynamic_local
        );
        let target = ControlTarget::new(destination, None);
        let _handshake = connect_or_spawn(&target, &launch)?;
        let fast_path = open_presentation_fast_path(&target, &[FrameTransportKind::SharedMemory])?;
        let backend = self.backend_registry.allocate(Some(fast_path.generation));
        let session_id = bound_session_for_active(backend, fast_path.active_session)
            .map(|(_backend, session)| session);
        let input =
            brood_ipc::CriticalProducer::map(fast_path.input_fd.as_fd(), fast_path.generation)?;
        let frames =
            brood_ipc::CriticalConsumer::map(fast_path.frame_fd.as_fd(), fast_path.generation)?;
        let catalog =
            brood_ipc::CriticalConsumer::map(fast_path.catalog_fd.as_fd(), fast_path.generation)?;
        spawn_frame_updates(
            target.clone(),
            frames,
            brood_ipc::EventNotifier::from_owned_fd(fast_path.frame_notify_fd),
            fast_path.generation,
            fast_path.profile,
            backend,
            self.event_proxy
                .clone()
                .context("event loop proxy is unavailable")?,
        )?;
        spawn_catalog_updates(
            catalog,
            brood_ipc::EventNotifier::from_owned_fd(fast_path.catalog_notify_fd),
            backend,
            self.event_proxy
                .clone()
                .context("event loop proxy is unavailable")?,
        )?;
        spawn_actor_message_updates(
            brood_ipc::ActorMessageConsumer::map(
                fast_path.presentation_message_fd.as_fd(),
                fast_path.generation,
            )?,
            brood_ipc::EventNotifier::from_owned_fd(fast_path.presentation_message_notify_fd),
            backend,
            self.event_proxy
                .clone()
                .context("event loop proxy is unavailable")?,
        )?;
        self.input_sender
            .as_mut()
            .context("presentation input lane is unavailable")?
            .insert_local(
                backend,
                LocalInputLane {
                    producer: input,
                    notifications: brood_ipc::EventNotifier::from_owned_fd(
                        fast_path.input_notify_fd,
                    ),
                    generation: fast_path.generation,
                    sequence: 0,
                    deferred: VecDeque::new(),
                    deferred_high_water: 0,
                    coalesced_motion: 0,
                    pane_routes: BTreeMap::new(),
                },
            )?;
        self.actor_message_sender
            .as_mut()
            .context("presentation actor lane is unavailable")?
            .insert_local(
                backend,
                LocalActorMessageLane {
                    producer: brood_ipc::ActorMessageProducer::map(
                        fast_path.actor_message_fd.as_fd(),
                        fast_path.generation,
                    )?,
                    notifications: brood_ipc::EventNotifier::from_owned_fd(
                        fast_path.actor_message_notify_fd,
                    ),
                    generation: fast_path.generation,
                    sequence: 0,
                    deferred: VecDeque::new(),
                },
            )?;
        self.backend_commands
            .as_mut()
            .context("presentation command lane is unavailable")?
            .insert_local(
                backend,
                LocalBackendCommandLane::start(
                    target,
                    self.event_proxy
                        .clone()
                        .context("event loop proxy is unavailable")?,
                )?,
            )?;
        self.backend_registry
            .observe(backend, Some(fast_path.generation));
        if let Some(runtime) = self.wasm_runtime.as_mut() {
            runtime.connect_backend(backend);
        }
        self.notify_backend_connected(backend)?;
        eprintln!("started dynamic local backend={backend:?} session={session_id:?}");
        Ok(())
    }

    fn connect_remote_backend(&mut self, address: String) -> Result<()> {
        let connect = address
            .to_socket_addrs()
            .with_context(|| format!("resolve remote backend address {address:?}"))?
            .next()
            .context("remote backend address resolved to no socket addresses")?;
        let mut config = self.remote_config_template.clone().context(
            "dynamic remote connections are unavailable without remote transport configuration",
        )?;
        config.connect = connect;
        self.queue_remote_backend_config(config, RemoteAttachOrigin::Direct { address })?;
        Ok(())
    }

    fn queue_remote_backend_config(
        &mut self,
        mut config: remote::RemoteConfig,
        origin: RemoteAttachOrigin,
    ) -> Result<BackendId> {
        let backend = self.backend_registry.allocate(None);
        config.backend = backend;
        let proxy = self
            .event_proxy
            .clone()
            .context("presentation event loop proxy is unavailable")?;
        let description = origin.description();
        thread::Builder::new()
            .name("brood-client-remote-attach".to_string())
            .spawn(move || {
                let mut origin = origin;
                match remote::RemoteConnection::start(config, &proxy) {
                    Ok(connection) => {
                        if let Err(winit::event_loop::EventLoopClosed(event)) =
                            proxy.send_event(ClientEvent::RemoteBackendReady(Box::new(
                                RemoteBackendReady { connection, origin },
                            )))
                            && let ClientEvent::RemoteBackendReady(ready) = event
                        {
                            let RemoteBackendReady {
                                mut connection,
                                mut origin,
                            } = *ready;
                            drop(connection.shutdown());
                            drop(origin.shutdown());
                        }
                    }
                    Err(error) => {
                        let mut message = format!("{error:#}");
                        if let Err(shutdown_error) = origin.shutdown() {
                            let _write_result =
                                write!(message, "; cleanup failed: {shutdown_error:#}");
                        }
                        drop(proxy.send_event(ClientEvent::RemoteBackendFailed {
                            backend,
                            description,
                            message,
                        }));
                    }
                }
            })
            .context("spawn dynamic remote backend attachment")?;
        Ok(backend)
    }

    fn accept_remote_backend(
        &mut self,
        mut connection: remote::RemoteConnection,
        mut origin: RemoteAttachOrigin,
    ) -> Result<()> {
        let backend = connection.backend;
        let attach_result = (|| {
            let (input, actor_messages, backend_commands) = connection.take_lanes()?;
            self.input_sender
                .as_mut()
                .context("presentation input lane is unavailable")?
                .insert_remote(backend, input)?;
            self.actor_message_sender
                .as_mut()
                .context("presentation actor lane is unavailable")?
                .insert_remote(backend, actor_messages)?;
            self.backend_commands
                .as_mut()
                .context("presentation command lane is unavailable")?
                .insert_remote(backend, backend_commands)?;
            self.backend_registry
                .observe(backend, Some(connection.generation));
            connection.activate_disconnect_notifications()?;
            if let Some(runtime) = self.wasm_runtime.as_mut() {
                runtime.connect_backend(backend);
            }
            self.notify_backend_connected(backend)
        })();
        if let Err(error) = attach_result {
            if let Some(input) = self.input_sender.as_mut() {
                let _removed = input.remove_backend(backend);
            }
            if let Some(actors) = self.actor_message_sender.as_mut() {
                let _removed = actors.remove_backend(backend);
            }
            if let Some(commands) = self.backend_commands.as_mut() {
                drop(commands.remove_backend(backend));
            }
            let _retired = self.backend_registry.retire(backend);
            if let Some(runtime) = self.wasm_runtime.as_mut() {
                drop(runtime.disconnect_backend(backend));
            }
            drop(connection.shutdown());
            drop(origin.shutdown());
            return Err(error);
        }
        match &mut origin {
            RemoteAttachOrigin::Direct { address } => {
                eprintln!("connected dynamic remote backend={backend:?} address={address}");
            }
            RemoteAttachOrigin::Ssh(deployment) => {
                deployment.backend = Some(backend);
                eprintln!(
                    "connected in-client SSH backend={backend:?} target={}",
                    deployment.target
                );
            }
            RemoteAttachOrigin::Container(deployment) => {
                deployment.backend = Some(backend);
                eprintln!(
                    "connected in-client container backend={backend:?} container={}",
                    deployment.description
                );
            }
        }
        self.remote_connections.push(connection);
        match origin {
            RemoteAttachOrigin::Direct { .. } => {}
            RemoteAttachOrigin::Ssh(deployment) => self.ssh_deployments.push(deployment),
            RemoteAttachOrigin::Container(deployment) => {
                self.container_deployments.push(deployment);
            }
        }
        Ok(())
    }

    fn connect_ssh_backend(&mut self, mut deployment: SshBackendDeployment) -> Result<()> {
        let config = (|| -> Result<remote::RemoteConfig> {
            let offer = format!(
                "{}={}",
                deployment.ready.session_package,
                deployment.ready.session_root.display()
            );
            Ok(remote::RemoteConfig {
                backend: BackendId(0),
                connect: deployment.ready.connect,
                server_name: deployment.ready.server_name.clone(),
                server_certificate: deployment.ready.server_certificate.clone(),
                client_certificate: deployment.ready.client_certificate.clone(),
                client_private_key: deployment.ready.client_private_key.clone(),
                artifact_offers: brood_transport::package_offers(&offer, ArtifactRealm::Session)?,
                allowed_client_artifacts: Vec::new(),
                artifact_cache: brood_transport::default_artifact_cache(),
            })
        })();
        match config {
            Ok(config) => self
                .queue_remote_backend_config(config, RemoteAttachOrigin::Ssh(deployment))
                .map(|_backend| ()),
            Err(error) => {
                if let Err(shutdown_error) = deployment.shutdown() {
                    eprintln!(
                        "could not stop failed SSH deployment {}: {shutdown_error:#}",
                        deployment.target
                    );
                }
                Err(error)
            }
        }
    }

    fn connect_container_backend(
        &mut self,
        mut deployment: ContainerBackendDeployment,
    ) -> Result<()> {
        let config = (|| -> Result<remote::RemoteConfig> {
            let offer = format!(
                "{}={}",
                deployment.ready.session_package,
                deployment.ready.session_root.display()
            );
            Ok(remote::RemoteConfig {
                backend: BackendId(0),
                connect: deployment.ready.connect,
                server_name: deployment.ready.server_name.clone(),
                server_certificate: deployment.ready.server_certificate.clone(),
                client_certificate: deployment.ready.client_certificate.clone(),
                client_private_key: deployment.ready.client_private_key.clone(),
                artifact_offers: brood_transport::package_offers(&offer, ArtifactRealm::Session)?,
                allowed_client_artifacts: Vec::new(),
                artifact_cache: brood_transport::default_artifact_cache(),
            })
        })();
        match config {
            Ok(config) => self
                .queue_remote_backend_config(config, RemoteAttachOrigin::Container(deployment))
                .map(|_backend| ()),
            Err(error) => {
                if let Err(shutdown_error) = deployment.shutdown() {
                    eprintln!(
                        "could not stop failed container deployment {}: {shutdown_error:#}",
                        deployment.description
                    );
                }
                Err(error)
            }
        }
    }

    fn start_ssh_backend(&self, target: String) -> Result<()> {
        let deployer = self
            .ssh_deployer
            .as_deref()
            .context("the brood-remote SSH deployer is unavailable")?;
        let proxy = self
            .event_proxy
            .clone()
            .context("presentation event loop proxy is unavailable")?;
        start_ssh_backend_deployment(deployer, target, proxy)
    }

    fn start_container_backend(&self, image: String, connection: Option<String>) -> Result<()> {
        let deployer = self
            .container_deployer
            .as_deref()
            .context("the brood-remote container deployer is unavailable")?;
        let proxy = self
            .event_proxy
            .clone()
            .context("presentation event loop proxy is unavailable")?;
        start_container_backend_deployment(deployer, &image, connection.as_deref(), proxy)
    }

    fn notify_backend_connected(&self, backend: BackendId) -> Result<()> {
        let Some(runtime) = self.wasm_runtime.as_ref() else {
            return Ok(());
        };
        let message = BackendCatalogueMessage::connected(backend);
        message.validate()?;
        runtime.send_actor_message(
            BACKEND_CATALOGUE_ACTOR_ID,
            BACKEND_CATALOGUE_CLIENT_SENDER.to_string(),
            serde_json::to_vec(&message).context("encode backend connection notification")?,
        )
    }

    fn notify_backend_connection_failed(&self, message: &str) -> Result<()> {
        let Some(runtime) = self.wasm_runtime.as_ref() else {
            return Ok(());
        };
        let notification = BackendCatalogueMessage::connection_failed(message);
        notification.validate()?;
        runtime.send_actor_message(
            BACKEND_CATALOGUE_ACTOR_ID,
            BACKEND_CATALOGUE_CLIENT_SENDER.to_string(),
            serde_json::to_vec(&notification).context("encode backend connection failure")?,
        )
    }

    fn publish_backend_sessions(&self) -> Result<()> {
        let Some(runtime) = self.wasm_runtime.as_ref() else {
            return Ok(());
        };
        let message = BackendCatalogueMessage::sessions(
            self.local_server_template.is_some(),
            self.ssh_deployer.is_some(),
            self.container_deployer.is_some(),
            self.backend_registry
                .connected_backend_ids()
                .map(|backend| BackendCatalogueEntry {
                    backend,
                    label: format!("Backend {}", backend.0),
                    sessions: self
                        .backend_sessions
                        .get(&backend)
                        .into_iter()
                        .flat_map(|snapshot| &snapshot.sessions)
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
                        })
                        .collect(),
                })
                .collect(),
        );
        message.validate()?;
        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some()
            && let BackendCatalogueMessage::Sessions { backends, .. } = &message
        {
            for backend in backends {
                if let Some(active) = backend.sessions.iter().find(|session| session.active) {
                    eprintln!(
                        "backend session catalogue backend={} active={} name={:?}",
                        backend.backend.0, active.id, active.name
                    );
                }
            }
        }
        runtime.send_actor_message(
            BACKEND_CATALOGUE_ACTOR_ID,
            BACKEND_CATALOGUE_CLIENT_SENDER.to_string(),
            serde_json::to_vec(&message).context("encode backend session catalogue")?,
        )
    }
}

struct PluginScrollBenchmark {
    pane: u64,
    started: Instant,
    first_input: Option<Instant>,
    last_input: Option<Instant>,
    input_intervals_us: Vec<u128>,
    first_scene: Option<Instant>,
    last_scene: Option<Instant>,
    scene_intervals_us: Vec<u128>,
    first_presented: Option<Instant>,
    last_presented: Option<Instant>,
    intervals_us: Vec<u128>,
    input_events: u64,
    redraws: u64,
    skipped: u64,
    reconfigures: u64,
    acquire_us: u128,
    encode_us: u128,
    submit_us: u128,
    present_us: u128,
    frames: u64,
    pending_scene: bool,
}

impl PluginScrollBenchmark {
    fn new(pane: u64) -> Self {
        Self {
            pane,
            started: Instant::now(),
            first_input: None,
            last_input: None,
            input_intervals_us: Vec::with_capacity(256),
            first_scene: None,
            last_scene: None,
            scene_intervals_us: Vec::with_capacity(256),
            first_presented: None,
            last_presented: None,
            intervals_us: Vec::with_capacity(256),
            input_events: 0,
            redraws: 0,
            skipped: 0,
            reconfigures: 0,
            acquire_us: 0,
            encode_us: 0,
            submit_us: 0,
            present_us: 0,
            frames: 0,
            pending_scene: false,
        }
    }

    fn record_input(&mut self, now: Instant) {
        record_cadence(
            now,
            &mut self.first_input,
            &mut self.last_input,
            &mut self.input_intervals_us,
        );
        self.input_events = self.input_events.saturating_add(1);
    }

    fn record_scene(&mut self, now: Instant) {
        record_cadence(
            now,
            &mut self.first_scene,
            &mut self.last_scene,
            &mut self.scene_intervals_us,
        );
        self.pending_scene = true;
    }

    fn record_present(&mut self, now: Instant, timings: RenderTimings) {
        if !self.pending_scene {
            return;
        }
        self.pending_scene = false;
        record_cadence(
            now,
            &mut self.first_presented,
            &mut self.last_presented,
            &mut self.intervals_us,
        );
        self.acquire_us = self.acquire_us.saturating_add(timings.acquire_us);
        self.encode_us = self.encode_us.saturating_add(timings.encode_us);
        self.submit_us = self.submit_us.saturating_add(timings.submit_us);
        self.present_us = self.present_us.saturating_add(timings.present_us);
        self.frames = self.frames.saturating_add(1);
    }

    fn record_redraw(&mut self) {
        self.redraws = self.redraws.saturating_add(1);
    }

    fn record_skipped(&mut self) {
        self.skipped = self.skipped.saturating_add(1);
    }

    fn record_reconfigure(&mut self) {
        self.reconfigures = self.reconfigures.saturating_add(1);
    }

    fn diagnostics(mut self) -> String {
        let input = cadence_diagnostics(
            &mut self.input_intervals_us,
            self.first_input,
            self.last_input,
            self.input_events,
        );
        let scenes = u64::try_from(self.scene_intervals_us.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(self.first_scene.is_some()));
        let scene = cadence_diagnostics(
            &mut self.scene_intervals_us,
            self.first_scene,
            self.last_scene,
            scenes,
        );
        let presented = cadence_diagnostics(
            &mut self.intervals_us,
            self.first_presented,
            self.last_presented,
            self.frames,
        );
        let within_budget = self
            .intervals_us
            .iter()
            .filter(|interval| **interval <= 16_667)
            .count();
        let within_180hz_budget = self
            .intervals_us
            .iter()
            .filter(|interval| **interval <= 5_556)
            .count();
        format!(
            "{{\"pane\":{},\"input_events\":{},\"input_fps_milli\":{},\"input_mean_interval_us\":{},\"input_p95_interval_us\":{},\"scenes\":{},\"scene_fps_milli\":{},\"scene_mean_interval_us\":{},\"scene_p95_interval_us\":{},\"redraws\":{},\"skipped\":{},\"reconfigures\":{},\"frames\":{},\"acquire_mean_us\":{},\"encode_mean_us\":{},\"submit_mean_us\":{},\"present_mean_us\":{},\"elapsed_us\":{},\"span_us\":{},\"fps_milli\":{},\"mean_interval_us\":{},\"p95_interval_us\":{},\"p99_interval_us\":{},\"max_interval_us\":{},\"within_5_556_us\":{},\"within_16_667_us\":{},\"intervals\":{}}}",
            self.pane,
            self.input_events,
            input.fps_milli,
            input.mean_interval_us,
            input.p95_interval_us,
            scenes,
            scene.fps_milli,
            scene.mean_interval_us,
            scene.p95_interval_us,
            self.redraws,
            self.skipped,
            self.reconfigures,
            self.frames,
            self.acquire_us / u128::from(self.frames.max(1)),
            self.encode_us / u128::from(self.frames.max(1)),
            self.submit_us / u128::from(self.frames.max(1)),
            self.present_us / u128::from(self.frames.max(1)),
            self.started.elapsed().as_micros(),
            presented.span_us,
            presented.fps_milli,
            presented.mean_interval_us,
            presented.p95_interval_us,
            presented.p99_interval_us,
            presented.max_interval_us,
            within_180hz_budget,
            within_budget,
            presented.intervals,
        )
    }

    fn scene_count(&self) -> u64 {
        u64::try_from(self.scene_intervals_us.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::from(self.first_scene.is_some()))
    }
}

struct CadenceDiagnostics {
    span_us: u128,
    fps_milli: u128,
    mean_interval_us: u128,
    p95_interval_us: u128,
    p99_interval_us: u128,
    max_interval_us: u128,
    intervals: usize,
}

fn record_cadence(
    now: Instant,
    first: &mut Option<Instant>,
    last: &mut Option<Instant>,
    intervals_us: &mut Vec<u128>,
) {
    if let Some(previous) = *last {
        intervals_us.push(now.duration_since(previous).as_micros());
    } else {
        *first = Some(now);
    }
    *last = Some(now);
}

fn cadence_diagnostics(
    intervals_us: &mut [u128],
    first: Option<Instant>,
    last: Option<Instant>,
    events: u64,
) -> CadenceDiagnostics {
    intervals_us.sort_unstable();
    let intervals = intervals_us.len();
    let span_us = first
        .zip(last)
        .map_or(0, |(first, last)| last.duration_since(first).as_micros());
    let percentile = |percent: usize| {
        if intervals == 0 {
            return 0;
        }
        let rank = intervals.saturating_mul(percent).div_ceil(100);
        intervals_us[rank.saturating_sub(1).min(intervals - 1)]
    };
    CadenceDiagnostics {
        span_us,
        fps_milli: if events <= 1 || span_us == 0 {
            0
        } else {
            u128::from(events - 1).saturating_mul(1_000_000_000) / span_us
        },
        mean_interval_us: if intervals == 0 {
            0
        } else {
            intervals_us.iter().sum::<u128>() / intervals as u128
        },
        p95_interval_us: percentile(95),
        p99_interval_us: percentile(99),
        max_interval_us: intervals_us.last().copied().unwrap_or(0),
        intervals,
    }
}

impl Application {
    fn accept_frame(&mut self, event: ClientEvent) -> Result<()> {
        let (backend, sequence, width, height, windows) = match event {
            ClientEvent::SceneFrame {
                backend,
                sequence,
                width,
                height,
                windows,
            } => {
                if !self.backend_registry.is_connected(backend) {
                    return Ok(());
                }
                let mut windows = windows;
                for window in &mut windows {
                    window.window_id = self.backend_registry.view_pane(QualifiedPane {
                        backend,
                        pane: window.window_id,
                    });
                }
                if let Some(sender) = self.input_sender.as_mut() {
                    sender.set_pane_routes(self.backend_registry.all_view_routes());
                }
                (backend, sequence, width, height, windows)
            }
            ClientEvent::TransportError(message) => return Err(anyhow!(message)),
            ClientEvent::BackendDisconnected { .. } => {
                return Err(anyhow!("backend disconnect reached scene transport path"));
            }
            ClientEvent::RemoteArtifactInstalled { .. } => {
                return Err(anyhow!(
                    "remote artifact event reached scene transport path"
                ));
            }
            ClientEvent::FocusPresentation => {
                return Err(anyhow!(
                    "presentation focus event reached scene transport path"
                ));
            }
            ClientEvent::ClosePresentation => {
                return Err(anyhow!(
                    "presentation close event reached scene transport path"
                ));
            }
            ClientEvent::PluginGraphicsChanged { .. } => {
                return Err(anyhow!(
                    "plugin graphics event reached scene transport path"
                ));
            }
            ClientEvent::RendererFailed { .. } => {
                return Err(anyhow!(
                    "renderer failure event reached scene transport path"
                ));
            }
            ClientEvent::BackendCommandCompleted { .. } => {
                return Err(anyhow!(
                    "backend command completion reached scene transport path"
                ));
            }
            ClientEvent::BackendSnapshot { .. } | ClientEvent::SessionCommandCompleted { .. } => {
                return Err(anyhow!("backend event reached scene transport path"));
            }
            ClientEvent::SshBackendReady(_) | ClientEvent::SshBackendFailed(_) => {
                return Err(anyhow!(
                    "SSH backend deployment event reached scene transport path"
                ));
            }
            ClientEvent::ContainerBackendReady(_) | ClientEvent::ContainerBackendFailed(_) => {
                return Err(anyhow!(
                    "container backend deployment event reached scene transport path"
                ));
            }
            ClientEvent::RemoteBackendReady(_) | ClientEvent::RemoteBackendFailed { .. } => {
                return Err(anyhow!(
                    "remote backend attachment event reached scene transport path"
                ));
            }
            ClientEvent::RemoteTerminalInputSent { .. } => {
                return Err(anyhow!(
                    "remote terminal timing event reached scene transport path"
                ));
            }
            ClientEvent::WaylandDroppedFile(_) => {
                return Err(anyhow!("Wayland file drop reached scene transport path"));
            }
            ClientEvent::ActorHostMessagesReady => {
                return Err(anyhow!("actor host message reached scene transport path"));
            }
            ClientEvent::BackendActorMessage { .. } => {
                return Err(anyhow!(
                    "session actor message reached scene transport path"
                ));
            }
        };
        let Some(renderer) = self.renderer.as_mut() else {
            self.pending_frames.insert(
                backend,
                ClientEvent::SceneFrame {
                    backend,
                    sequence,
                    width,
                    height,
                    windows,
                },
            );
            return Ok(());
        };
        let (focus_change, released_resources) =
            renderer.select_scene(backend, sequence, width, height, windows)?;
        if let Some(commands) = self.backend_commands.as_ref() {
            for resource in released_resources {
                commands.release_resource_to(backend, resource)?;
            }
        }
        renderer.sync_plugin_graphics_extents()?;
        if self
            .pointer_grab
            .as_ref()
            .is_some_and(|grab| renderer.focus_target_for_pane(grab.target.pane()).is_none())
        {
            // The grabbed pane may close before Winit reports the physical
            // release. End the client-side grab now; the backend deliberately
            // accepts this release even after removing the pane inventory.
            cancel_pointer_grab(
                self.wasm_runtime.as_ref(),
                self.input_sender.as_mut(),
                &mut self.pointer_routing,
                &mut self.pointer_grab,
                modifier_bits(self.modifiers),
            );
            route_pointer_motion(
                renderer,
                self.cursor_position,
                &mut self.pointer_grab,
                self.wasm_runtime.as_ref(),
                self.input_sender.as_mut(),
                &mut self.pointer_routing,
            );
        }
        if let Some(change) = focus_change {
            dispatch_focus_change(
                self.wasm_runtime.as_ref(),
                self.input_sender.as_mut(),
                change,
                self.window_focused,
            )?;
        }
        flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
        flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)?;
        Ok(())
    }

    fn publish_layout_state(&mut self) -> Result<()> {
        if self.window_resize.is_some() {
            return Ok(());
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(());
        };
        let live_actors = self
            .wasm_runtime
            .as_ref()
            .map_or_else(Vec::new, PresentationWasmRuntime::actor_identities);
        loop {
            let failed = self.layout_leases.lease_tokens().into_iter().find(|lease| {
                self.layout_holders
                    .get(&lease.id)
                    .is_some_and(|holder| !live_actors.contains(holder))
            });
            let Some(failed) = failed else {
                break;
            };
            let was_root = failed.id == self.layout_leases.root_token().id;
            let promoted = self.layout_leases.rest_token(failed).and_then(|child| {
                self.layout_holders
                    .get(&child.id)
                    .cloned()
                    .map(|holder| (child, holder))
            });
            if let Some((child, holder)) = promoted {
                let promoted = self.layout_leases.collapse_wrapper(failed)?;
                self.layout_holders.remove(&failed.id);
                self.layout_holders.insert(promoted.id, holder.clone());
                if was_root {
                    self.root_layout_actor = Some(holder.clone());
                }
                self.layout_publications.clear();
                renderer.layout_state_dirty = true;
                eprintln!(
                    "collapsed failed layout wrapper lease={} and promoted lease={} actor={}",
                    failed.id.0, child.id.0, holder.name
                );
                continue;
            }

            eprintln!(
                "revoked layout tree after leaf actor failure lease={}",
                failed.id.0
            );
            self.root_layout_actor = None;
            self.layout_holders.clear();
            self.layout_publications.clear();
            self.layout_leases = LayoutLeaseTree::default();
            renderer.layout_state_dirty = true;
            break;
        }
        let candidates = self
            .wasm_runtime
            .as_ref()
            .map_or_else(Vec::new, PresentationWasmRuntime::layout_actor_identities);
        let selected = self
            .root_layout_actor
            .as_ref()
            .filter(|holder| {
                self.wasm_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.actor_identity(&holder.name))
                    .as_ref()
                    == Some(holder)
            })
            .cloned()
            .or_else(|| candidates.first().cloned());
        if selected != self.root_layout_actor {
            self.root_layout_actor = selected;
            self.layout_leases = LayoutLeaseTree::default();
            self.layout_holders.clear();
            self.layout_publications.clear();
            if let Some(holder) = &self.root_layout_actor {
                self.layout_holders
                    .insert(self.layout_leases.root_token().id, holder.clone());
                renderer.layout_state_dirty = true;
                eprintln!(
                    "granted root layout lease actor={} actor_generation={} lease_generation={}",
                    holder.name,
                    holder.generation,
                    self.layout_leases.root_token().generation
                );
            }
        }
        let Some(holder) = self.root_layout_actor.clone() else {
            return Ok(());
        };
        if candidates.len() > 1 {
            let ignored = candidates
                .iter()
                .filter(|candidate| **candidate != holder)
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>();
            eprintln!(
                "root layout lease is exclusive holder={} ignored={ignored:?}",
                holder.name
            );
        }

        if renderer.layout_state_dirty {
            self.layout_revision = self
                .layout_revision
                .checked_add(1)
                .context("layout state revision limit exceeded")?;
            let panes = renderer.pane_inventory();
            let previously_owned = self
                .layout_leases
                .lease_tokens()
                .into_iter()
                .flat_map(|token| self.layout_leases.owned_panes(token).unwrap_or_default())
                .collect::<HashSet<_>>();
            let new_global_panes = newly_declared_global_plugin_panes(
                &previously_owned,
                &panes,
                &renderer.plugin_pane_realms,
            );
            self.layout_leases
                .update_inventory(self.layout_revision, &panes)?;
            if !new_global_panes.is_empty() {
                self.layout_leases
                    .assign_panes(self.layout_leases.root_token(), &new_global_panes)?;
            }
            if let Some(runtime) = self.wasm_runtime.as_ref() {
                for token in self.layout_leases.lease_tokens() {
                    let Some(holder) = self.layout_holders.get(&token.id) else {
                        continue;
                    };
                    let Some(pane) = runtime.pane_for_actor(holder) else {
                        continue;
                    };
                    if panes.contains(&pane) {
                        self.layout_leases.assign_panes(token, &[pane])?;
                    }
                }
            }
            self.layout_publications.clear();
            renderer.layout_state_dirty = false;
        }
        if self.layout_revision == 0 {
            return Ok(());
        }

        let tokens = self.layout_leases.lease_tokens();
        for lease in tokens {
            if self.layout_publications.get(&lease.id) == Some(&self.layout_revision) {
                continue;
            }
            let Some(viewport) = self.layout_leases.viewport_transform(lease) else {
                continue;
            };
            let Some(holder) = self.layout_holders.get(&lease.id).cloned() else {
                continue;
            };
            let owned_panes = self
                .layout_leases
                .owned_panes(lease)
                .context("layout lease is unavailable during state publication")?;
            let focused_pane = renderer
                .focused_pane_id()
                .filter(|pane| self.layout_leases.subtree_contains(lease, *pane));
            let viewport = viewport.pixel_rect(renderer.layout_extent());
            let message = renderer.layout_state_message(
                self.layout_revision,
                lease,
                &owned_panes,
                focused_pane,
                LayoutSize {
                    w: viewport.w,
                    h: viewport.h,
                },
                self.layout_leases.has_rest(lease).unwrap_or(false),
            );
            if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                let mut backends = message
                    .pane_realms
                    .iter()
                    .map(|realm| realm.backend.0)
                    .collect::<Vec<_>>();
                backends.sort_unstable();
                backends.dedup();
                eprintln!(
                    "layout-state backend groups lease={} actor={} backends={backends:?}",
                    lease.id.0, holder.name
                );
            }
            let payload =
                serde_json::to_vec(&message).context("encode client layout-state message")?;
            let result = self.wasm_runtime.as_ref().map_or(Ok(()), |runtime| {
                runtime.send_layout_state_to(&holder, payload)
            });
            if let Err(error) = result {
                eprintln!(
                    "deferred layout-state revision={} lease={} actor={} after mailbox error: {error:#}",
                    self.layout_revision, lease.id.0, holder.name
                );
                continue;
            }
            self.layout_publications
                .insert(lease.id, self.layout_revision);
            eprintln!(
                "queued layout-state revision={} lease={} actor={}",
                self.layout_revision, lease.id.0, holder.name
            );
        }
        Ok(())
    }

    fn apply_actor_host_messages(&mut self) -> Result<()> {
        let messages = self
            .wasm_runtime
            .as_mut()
            .map_or(Ok(Vec::new()), PresentationWasmRuntime::drain_host_messages)?;
        for actor_message in messages {
            let message: HostMessage = match serde_json::from_slice(&actor_message.payload) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("rejected malformed presentation actor host message: {error}");
                    continue;
                }
            };
            let focus_change = match message {
                HostMessage::ClaimRootLayout => {
                    let Some(sender) = actor_message.sender.as_ref() else {
                        eprintln!("ignored root layout claim without authenticated actor identity");
                        continue;
                    };
                    let authorized = self
                        .wasm_runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.can_claim_root_layout(sender));
                    if !authorized {
                        eprintln!(
                            "ignored unauthorized root layout claim actor={} actor_generation={}",
                            sender.name, sender.generation
                        );
                        continue;
                    }
                    if self.root_layout_actor.as_ref() != Some(sender) {
                        self.layout_leases = LayoutLeaseTree::default();
                        self.layout_holders.clear();
                        self.layout_publications.clear();
                        self.root_layout_actor = Some(sender.clone());
                        let root = self.layout_leases.root_token();
                        self.layout_holders.insert(root.id, sender.clone());
                        if let Some(renderer) = self.renderer.as_mut() {
                            renderer.layout_state_dirty = true;
                        }
                        eprintln!(
                            "granted claimed root layout lease actor={} actor_generation={} lease_generation={}",
                            sender.name, sender.generation, root.generation
                        );
                    }
                    None
                }
                HostMessage::DelegateLayoutRest {
                    lease,
                    revision,
                    actor,
                    panes,
                } => {
                    let Some(sender) = actor_message.sender.as_ref() else {
                        eprintln!("ignored layout delegation without authenticated actor identity");
                        continue;
                    };
                    let lease = convert_actor_layout_lease(lease);
                    if self.layout_holders.get(&lease.id) != Some(sender) {
                        eprintln!(
                            "ignored layout delegation from non-holder actor={} lease={}",
                            sender.name, lease.id.0
                        );
                        continue;
                    }
                    if revision != self.layout_leases.inventory_revision() {
                        eprintln!(
                            "ignored stale layout delegation revision={revision}; current revision={}",
                            self.layout_leases.inventory_revision()
                        );
                        continue;
                    }
                    let Some(child_holder) = self
                        .wasm_runtime
                        .as_ref()
                        .and_then(|runtime| runtime.actor_identity(&actor))
                    else {
                        eprintln!("ignored layout delegation to unavailable actor={actor}");
                        continue;
                    };
                    match self.layout_leases.grant_rest(lease, &panes) {
                        Ok(child) => {
                            self.layout_holders.insert(child.id, child_holder.clone());
                            self.layout_publications.remove(&child.id);
                            eprintln!(
                                "granted rest layout lease parent={} child={} actor={} panes={}",
                                lease.id.0,
                                child.id.0,
                                child_holder.name,
                                panes.len()
                            );
                        }
                        Err(error) => {
                            eprintln!("rejected rest layout delegation: {error}");
                        }
                    }
                    None
                }
                HostMessage::SetResolvedLayout {
                    lease,
                    revision,
                    layout,
                    rest,
                    focus,
                } => {
                    let Some(sender) = actor_message.sender.as_ref() else {
                        eprintln!("ignored layout result without authenticated actor identity");
                        continue;
                    };
                    let Some(lease) = lease else {
                        let Some(renderer) = self.renderer.as_mut() else {
                            continue;
                        };
                        renderer.reject_actor_layout(&"layout result is missing its lease token");
                        continue;
                    };
                    let submitted_lease = convert_actor_layout_lease(lease);
                    let Some(holder) = self.layout_holders.get(&submitted_lease.id) else {
                        eprintln!(
                            "ignored layout result because lease={} has no holder",
                            submitted_lease.id.0
                        );
                        continue;
                    };
                    if sender != holder {
                        eprintln!(
                            "ignored layout result from non-holder actor={} actor_generation={} lease={}",
                            sender.name, sender.generation, submitted_lease.id.0
                        );
                        continue;
                    }
                    let Some(revision) = revision else {
                        let Some(renderer) = self.renderer.as_mut() else {
                            continue;
                        };
                        renderer
                            .reject_actor_layout(&"layout result is missing its input revision");
                        continue;
                    };
                    let current_lease = self
                        .layout_leases
                        .lease_tokens()
                        .into_iter()
                        .find(|token| token.id == submitted_lease.id);
                    if current_lease != Some(submitted_lease) {
                        eprintln!(
                            "ignored stale layout result lease={} generation={}; current generation={}",
                            submitted_lease.id.0,
                            submitted_lease.generation,
                            current_lease.map_or(0, |lease| lease.generation)
                        );
                        continue;
                    }
                    if revision != self.layout_leases.inventory_revision() {
                        eprintln!(
                            "ignored stale layout result revision={revision}; current revision={}",
                            self.layout_leases.inventory_revision()
                        );
                        continue;
                    }
                    let result = (|| {
                        self.layout_leases.submit(
                            submitted_lease,
                            revision,
                            convert_actor_layout_fragment(layout, rest),
                            focus,
                        )?;
                        if self.layout_leases.is_complete() {
                            self.layout_leases
                                .compose()
                                .map(Some)
                                .map_err(anyhow::Error::from)
                        } else {
                            Ok(None)
                        }
                    })();
                    let Some(renderer) = self.renderer.as_mut() else {
                        continue;
                    };
                    let (focus_change, accepted) = match result {
                        Ok(Some(composed)) => {
                            renderer.commit_actor_layout(composed.layout, composed.focus)
                        }
                        Ok(None) => {
                            eprintln!(
                                "accepted layout fragment lease={} revision={revision}; waiting for child",
                                submitted_lease.id.0
                            );
                            (None, false)
                        }
                        Err(error) => renderer.reject_actor_layout(&error),
                    };
                    if accepted {
                        eprintln!(
                            "accepted leased actor layout items={} panes={} revision={}",
                            renderer.accepted_layout.items.len(),
                            renderer.accepted_layout.pane_count(),
                            self.layout_leases.inventory_revision()
                        );
                    }
                    focus_change
                }
                HostMessage::BackendSession {
                    backend,
                    request,
                    operation,
                    session,
                    name,
                    directory,
                } => {
                    let operation = match operation.as_str() {
                        "refresh" => SessionOperation::Refresh,
                        "activate" => SessionOperation::Activate,
                        "create" => SessionOperation::Create,
                        "rename" => SessionOperation::Rename,
                        "close" => SessionOperation::Close,
                        other => {
                            eprintln!("rejected unknown backend session operation {other:?}");
                            continue;
                        }
                    };
                    let actor_id = actor_message
                        .sender
                        .as_ref()
                        .map(|sender| sender.name.clone())
                        .unwrap_or_else(|| BACKEND_CATALOGUE_ACTOR_ID.to_string());
                    let command = actor_runtime::SessionCommandEnvelope {
                        actor_id,
                        request: SessionCommandRequest {
                            backend_id: backend,
                            request,
                            operation,
                            session,
                            name,
                            directory,
                        },
                    };
                    self.backend_commands
                        .as_ref()
                        .context("backend session command requires a connected backend")?
                        .try_send_session_to(BackendId(backend), command)?;
                    None
                }
                HostMessage::ConnectRemoteBackend { address } => {
                    if let Err(error) = self.connect_remote_backend(address) {
                        eprintln!("could not connect dynamic remote backend: {error:#}");
                        if let Err(notify_error) =
                            self.notify_backend_connection_failed(&format!("{error:#}"))
                        {
                            eprintln!(
                                "could not notify session switcher about remote backend failure: \
                                 {notify_error:#}"
                            );
                        }
                    }
                    None
                }
                HostMessage::DeploySshBackend { target } => {
                    if let Err(error) = self.start_ssh_backend(target) {
                        eprintln!("could not start in-client SSH backend: {error:#}");
                        if let Err(notify_error) =
                            self.notify_backend_connection_failed(&format!("{error:#}"))
                        {
                            eprintln!(
                                "could not notify session switcher about SSH backend failure: \
                                 {notify_error:#}"
                            );
                        }
                    }
                    None
                }
                HostMessage::DeployContainerBackend { image, connection } => {
                    if let Err(error) = self.start_container_backend(image, connection) {
                        eprintln!("could not start in-client container backend: {error:#}");
                        if let Err(notify_error) =
                            self.notify_backend_connection_failed(&format!("{error:#}"))
                        {
                            eprintln!(
                                "could not notify session switcher about container backend \
                                 failure: {notify_error:#}"
                            );
                        }
                    }
                    None
                }
                HostMessage::RequestBackendCatalogue => {
                    if let Err(error) = self.publish_backend_sessions() {
                        eprintln!("could not publish requested backend catalogue: {error:#}");
                    }
                    None
                }
                HostMessage::CreateLocalBackend => {
                    if let Err(error) = self.connect_local_backend() {
                        eprintln!("could not start dynamic local backend: {error:#}");
                        if let Err(notify_error) =
                            self.notify_backend_connection_failed(&format!("{error:#}"))
                        {
                            eprintln!(
                                "could not notify session switcher about local backend failure: \
                                 {notify_error:#}"
                            );
                        }
                    }
                    None
                }
                HostMessage::SetKeybinding {
                    action,
                    key_code,
                    modifiers,
                } => {
                    let Some(sender) = actor_message.sender.as_ref() else {
                        eprintln!("ignored keybinding update without authenticated actor identity");
                        continue;
                    };
                    if sender.name != "keybindings" {
                        eprintln!(
                            "ignored keybinding update from unauthorized actor={}",
                            sender.name
                        );
                        continue;
                    }
                    let result = self
                        .key_policy
                        .as_mut()
                        .context("presentation key policy is unavailable")
                        .and_then(|policy| {
                            policy.set_binding(
                                &action,
                                brood_keybindings::Binding::new(key_code, modifiers),
                            )
                        });
                    let (success, message) = match &result {
                        Ok(()) => (true, "Saved".to_string()),
                        Err(error) => (false, format!("{error:#}")),
                    };
                    if let Some(runtime) = self.wasm_runtime.as_ref() {
                        let response = serde_json::to_vec(&serde_json::json!({
                            "type": "keybinding-saved",
                            "action": action,
                            "key_code": key_code,
                            "modifiers": modifiers,
                            "success": success,
                            "message": message,
                        }))?;
                        if let Err(error) = runtime.send_key_policy_message("keybindings", response)
                        {
                            eprintln!("could not acknowledge keybinding update: {error:#}");
                        }
                    }
                    if let Err(error) = result {
                        eprintln!("could not persist keybinding update: {error:#}");
                    }
                    None
                }
                message => {
                    let Some(renderer) = self.renderer.as_mut() else {
                        continue;
                    };
                    if let Err(error) = apply_key_policy_host_message(
                        renderer,
                        &mut self.input_sender,
                        self.backend_commands.as_ref(),
                        self.wasm_runtime.as_mut(),
                        self.clipboard.as_ref(),
                        message,
                        self.window_focused,
                    ) {
                        eprintln!("rejected presentation actor host message: {error:#}");
                    }
                    None
                }
            };
            if let Some(change) = focus_change {
                dispatch_focus_change(
                    self.wasm_runtime.as_ref(),
                    self.input_sender.as_mut(),
                    change,
                    self.window_focused,
                )?;
            }
            let Some(renderer) = self.renderer.as_mut() else {
                continue;
            };
            renderer.sync_plugin_graphics_extents()?;
            flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
            flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)?;
            renderer.window.request_redraw();
        }
        Ok(())
    }

    fn drain_wasm_events(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let result = (|| {
            let Some(runtime) = self.wasm_runtime.as_mut() else {
                return Ok(());
            };
            runtime.drain_events()?;
            let pane_sessions = runtime.pane_realms().collect::<Vec<_>>();
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_plugin_pane_realms(&pane_sessions);
            }
            if let Some(renderer) = self.renderer.as_ref() {
                let (width, height, scale) = renderer.logical_extent();
                let panes = runtime.drain_pending_pane_creations().collect::<Vec<_>>();
                for pane in panes {
                    runtime.send_presentation_event(PresentationEvent::PaneCreated {
                        pane,
                        width,
                        height,
                        scale,
                    })?;
                }
            }
            let panes = runtime.drain_pending_pane_removals().collect::<Vec<_>>();
            if let Some(renderer) = self.renderer.as_mut() {
                for pane in panes {
                    let change = renderer.remove_plugin_pane(pane);
                    if let Some(change) = change {
                        dispatch_focus_change(
                            Some(runtime),
                            self.input_sender.as_mut(),
                            change,
                            self.window_focused,
                        )?;
                    }
                }
            }
            let messages = runtime.drain_backend_messages();
            let commands = runtime.drain_session_commands();
            if !commands.is_empty() {
                let lane = self
                    .backend_commands
                    .as_ref()
                    .context("presentation actor requested session control without a backend")?;
                for command in commands {
                    lane.try_send_session_to(BackendId(command.request.backend_id), command)?;
                }
            }
            if !messages.is_empty() {
                let actor_panes = runtime
                    .pane_actor_ids()
                    .map(|(pane, actor)| (actor.to_string(), pane))
                    .collect::<HashMap<_, _>>();
                let lane = self
                    .actor_message_sender
                    .as_mut()
                    .context("presentation actor requested a backend message without a backend")?;
                for message in &messages {
                    if let Some(trace_sequence) = terminal_input_sequence(&message.body)
                        && let Some(pane) = actor_panes.get(&message.sender).copied()
                        && let Some(input) = self.pending_plugin_inputs.iter_mut().find(|input| {
                            input.pane == pane
                                && input.backend == Some(message.backend)
                                && input.trace_sequence.is_none()
                        })
                    {
                        input.trace_sequence = Some(trace_sequence);
                    }
                    if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some()
                        && !PLUGIN_SCROLL_BENCH_ACTIVE.load(Ordering::Relaxed)
                    {
                        eprintln!(
                            "presentation actor message realm={:?} sender={} recipient={} body={}",
                            message.realm,
                            message.sender,
                            message.actor,
                            String::from_utf8_lossy(&message.body)
                        );
                    }
                    lane.send(message)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.error = Some(error);
            event_loop.exit();
            return false;
        }
        true
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.error = Some(error);
        event_loop.exit();
    }

    fn sync_default_backend_target(&mut self) {
        let Some(pane) = self.renderer.as_ref().and_then(Renderer::focused_pane_id) else {
            return;
        };
        let Some(qualified) = self.backend_registry.routable_backend_pane(pane) else {
            return;
        };
        if let Some(commands) = self.backend_commands.as_mut() {
            commands.set_default_backend(qualified.backend);
        }
        if let Some(input) = self.input_sender.as_mut() {
            input.set_default_backend(qualified.backend);
        }
        if let Some(messages) = self.actor_message_sender.as_mut() {
            messages.set_default_backend(qualified.backend);
        }
    }

    fn settle_window_resize(&mut self) -> Result<()> {
        let Some(transaction) = self.window_resize.as_ref() else {
            return Ok(());
        };
        if !transaction.ready(Instant::now()) {
            return Ok(());
        }
        let transaction = self
            .window_resize
            .take()
            .context("window resize transaction disappeared while settling")?;
        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(());
        };
        let focus_change = renderer.finish_window_resize()?;
        if let Some(change) = focus_change {
            dispatch_focus_change(
                self.wasm_runtime.as_ref(),
                self.input_sender.as_mut(),
                change,
                self.window_focused,
            )?;
        }
        if let Some(sender) = self.input_sender.as_mut() {
            send_presentation_configuration(sender, renderer);
        }
        flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
        flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)?;
        eprintln!(
            "settled presentation window resize events={} from={}x{} to={}x{} elapsed_ms={}",
            transaction.events,
            transaction.initial_size.0,
            transaction.initial_size.1,
            transaction.latest_size.0,
            transaction.latest_size.1,
            transaction.started.elapsed().as_millis(),
        );
        Ok(())
    }

    fn allocate_renderer_generation(&mut self) -> Result<u64> {
        self.next_renderer_generation = self
            .next_renderer_generation
            .checked_add(1)
            .context("renderer generation limit exceeded")?;
        Ok(self.next_renderer_generation)
    }

    fn recover_renderer(&mut self, failed_generation: u64, message: &str) -> Result<()> {
        let Some(current) = self.renderer.as_ref() else {
            return Ok(());
        };
        if current.generation != failed_generation {
            return Ok(());
        }
        self.renderer_recoveries = self.renderer_recoveries.saturating_add(1);
        if self.renderer_recoveries > 3 {
            return Err(anyhow!(
                "renderer recovery failed repeatedly after: {message}"
            ));
        }
        let old = self
            .renderer
            .take()
            .context("lost renderer during recovery")?;
        self.window_resize = None;
        let replay = old
            .plugin_panes
            .values()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence))
            .collect::<Vec<_>>();
        let available_windows = old.available_windows.clone();
        let active_session = old.active_session;
        let window = Arc::clone(&old.window);
        drop(old);
        let generation = self.allocate_renderer_generation()?;
        let proxy = self
            .event_proxy
            .clone()
            .context("renderer recovery event proxy is unavailable")?;
        eprintln!(
            "recovering renderer generation={failed_generation} replacement={generation}: {message}"
        );
        let graphics = self
            .plugin_graphics
            .clone()
            .context("plugin graphics bridge is unavailable")?;
        let mut replacement =
            Renderer::new(window, self.fast_path_enabled, graphics, generation, proxy)?;
        replacement.available_windows = available_windows;
        let _focus_change = replacement.apply_default_session_layout(active_session);
        for (pane, sequence) in replay {
            replacement.refresh_plugin_graphics(pane, sequence)?;
        }
        replacement.window.request_redraw();
        self.renderer = Some(replacement);
        if let Some(renderer) = self.renderer.as_mut() {
            flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)?;
        }
        Ok(())
    }
}

impl ApplicationHandler<ClientEvent> for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }

        let title = if self.backend_connected {
            "Brood"
        } else {
            "Brood - No backend"
        };
        let generation = match self.allocate_renderer_generation() {
            Ok(generation) => generation,
            Err(error) => {
                self.fail(event_loop, error);
                return;
            }
        };
        let Some(proxy) = self.event_proxy.clone() else {
            self.fail(event_loop, anyhow!("renderer event proxy is unavailable"));
            return;
        };
        let result = event_loop
            .create_window(
                Window::default_attributes()
                    .with_title(title)
                    .with_resizable(true)
                    .with_inner_size(LogicalSize::new(
                        DEFAULT_WINDOW_WIDTH,
                        DEFAULT_WINDOW_HEIGHT,
                    ))
                    .with_min_inner_size(LogicalSize::new(480.0, 320.0)),
            )
            .context("create presentation window")
            .and_then(|window| {
                let graphics = self
                    .plugin_graphics
                    .clone()
                    .context("plugin graphics bridge is unavailable")?;
                Renderer::new(
                    Arc::new(window),
                    self.fast_path_enabled,
                    graphics,
                    generation,
                    proxy.clone(),
                )
            });

        match result {
            Ok(renderer) => {
                match WaylandDropBridge::new(renderer.window.as_ref(), move |drop_event| {
                    drop(proxy.send_event(ClientEvent::WaylandDroppedFile(drop_event)));
                }) {
                    Ok(bridge) => self.wayland_drop = bridge,
                    Err(error) => {
                        self.fail(
                            event_loop,
                            error.context("initialize presentation file drop"),
                        );
                        return;
                    }
                }
                renderer.window.request_redraw();
                self.renderer = Some(renderer);
                if let (Some(runtime), Some(renderer)) =
                    (self.wasm_runtime.as_ref(), self.renderer.as_mut())
                {
                    let sessions = runtime.pane_realms().collect::<Vec<_>>();
                    renderer.set_plugin_pane_realms(&sessions);
                }
                if let Some(renderer) = self.renderer.as_mut() {
                    for (backend, (active_session, windows)) in
                        std::mem::take(&mut self.pending_backend_snapshots)
                    {
                        let _focus_change =
                            renderer.select_backend_snapshot(backend, active_session, windows);
                    }
                }
                if let (Some(sender), Some(renderer)) =
                    (self.input_sender.as_mut(), self.renderer.as_ref())
                {
                    send_presentation_configuration(sender, renderer);
                }
                for frame in std::mem::take(&mut self.pending_frames).into_values() {
                    if let Err(error) = self.accept_frame(frame) {
                        self.error = Some(error);
                        event_loop.exit();
                        return;
                    }
                }
                if let Some(renderer) = self.renderer.as_ref() {
                    let (width, height, scale) = renderer.logical_extent();
                    if let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::PaneCreated {
                                pane,
                                width,
                                height,
                                scale,
                            }
                        })
                    {
                        self.fail(event_loop, error);
                        return;
                    }
                    if let Err(error) =
                        send_presentation_events(self.wasm_runtime.as_ref(), |pane| {
                            PresentationEvent::PaneFocused {
                                pane,
                                focused: true,
                            }
                        })
                    {
                        self.fail(event_loop, error);
                        return;
                    }
                    if let Some(runtime) = self.wasm_runtime.as_mut() {
                        drop(runtime.drain_pending_pane_creations());
                    }
                }
            }
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ClientEvent) {
        if !self.drain_wasm_events(event_loop) {
            return;
        }
        match event {
            ClientEvent::WaylandDroppedFile(drop) => {
                if let Some(renderer) = self.renderer.as_ref() {
                    deliver_dropped_image(
                        renderer,
                        self.wasm_runtime.as_ref(),
                        Some((f64::from(drop.position.0), f64::from(drop.position.1))),
                        &drop.path,
                    );
                }
            }
            ClientEvent::RemoteArtifactInstalled {
                manifest,
                path,
                completion,
            } => {
                let result = (|| {
                    anyhow::ensure!(
                        manifest.realm == ArtifactRealm::Client,
                        "remote artifact {:?} targets the wrong realm",
                        manifest.id
                    );
                    let pane = self
                        .wasm_runtime
                        .as_mut()
                        .context("client WASM runtime is unavailable")?
                        .install_artifact_actor(&manifest.id, &path)?;
                    eprintln!(
                        "installed remote client artifact id={} pane={} digest={}",
                        manifest.id,
                        pane,
                        brood_transport::digest_hex(manifest.digest)
                    );
                    Ok::<(), anyhow::Error>(())
                })();
                let completion_result = match &result {
                    Ok(()) => Ok(()),
                    Err(error) => Err(format!("{error:#}")),
                };
                drop(completion.send(completion_result));
                if let Err(error) = result {
                    eprintln!("rejected remote client artifact: {error:#}");
                }
            }
            ClientEvent::FocusPresentation => {
                if let Some(renderer) = self.renderer.as_ref() {
                    renderer
                        .window
                        .request_user_attention(Some(UserAttentionType::Critical));
                    renderer.window.focus_window();
                }
            }
            ClientEvent::ClosePresentation => event_loop.exit(),
            ClientEvent::BackendDisconnected { backend, message } => {
                match self.disconnect_remote_backend(backend, &message) {
                    Ok(focus_change) => {
                        if let Some(change) = focus_change
                            && let Err(error) = dispatch_focus_change(
                                self.wasm_runtime.as_ref(),
                                self.input_sender.as_mut(),
                                change,
                                self.window_focused,
                            )
                        {
                            self.fail(event_loop, error);
                            return;
                        }
                        if let Err(error) = self.publish_layout_state() {
                            self.fail(event_loop, error);
                        }
                    }
                    Err(error) => self.fail(event_loop, error),
                }
            }
            ClientEvent::RemoteBackendReady(ready) => {
                let RemoteBackendReady { connection, origin } = *ready;
                if let Err(error) = self.accept_remote_backend(connection, origin) {
                    eprintln!("could not attach remote backend: {error:#}");
                    if let Err(notify_error) =
                        self.notify_backend_connection_failed(&format!("{error:#}"))
                    {
                        eprintln!(
                            "could not notify session switcher about remote backend failure: \
                             {notify_error:#}"
                        );
                    }
                }
            }
            ClientEvent::RemoteBackendFailed {
                backend,
                description,
                message,
            } => {
                let _retired = self.backend_registry.retire(backend);
                eprintln!("{description} attachment failed: {message}");
                if let Err(error) = self.notify_backend_connection_failed(&message) {
                    eprintln!(
                        "could not notify session switcher about remote backend failure: {error:#}"
                    );
                }
            }
            ClientEvent::SshBackendReady(deployment) => {
                if let Err(error) = self.connect_ssh_backend(deployment) {
                    eprintln!("could not attach deployed SSH backend: {error:#}");
                    if let Err(notify_error) =
                        self.notify_backend_connection_failed(&format!("{error:#}"))
                    {
                        eprintln!(
                            "could not notify session switcher about SSH backend failure: \
                             {notify_error:#}"
                        );
                    }
                }
            }
            ClientEvent::SshBackendFailed(message) => {
                eprintln!("in-client SSH backend deployment failed: {message}");
                if let Err(error) = self.notify_backend_connection_failed(&message) {
                    eprintln!(
                        "could not notify session switcher about SSH backend failure: {error:#}"
                    );
                }
            }
            ClientEvent::ContainerBackendReady(deployment) => {
                if let Err(error) = self.connect_container_backend(deployment) {
                    eprintln!("could not attach deployed container backend: {error:#}");
                    if let Err(notify_error) =
                        self.notify_backend_connection_failed(&format!("{error:#}"))
                    {
                        eprintln!(
                            "could not notify session switcher about container backend failure: \
                             {notify_error:#}"
                        );
                    }
                }
            }
            ClientEvent::ContainerBackendFailed(message) => {
                eprintln!("in-client container backend deployment failed: {message}");
                if let Err(error) = self.notify_backend_connection_failed(&message) {
                    eprintln!(
                        "could not notify session switcher about container backend failure: \
                         {error:#}"
                    );
                }
            }
            ClientEvent::RemoteTerminalInputSent {
                backend,
                trace_sequence,
                queued_at,
                sent_at,
            } => {
                for input in &mut self.pending_plugin_inputs {
                    if input.backend == Some(backend)
                        && input.trace_sequence == Some(trace_sequence)
                        && input.transport_sent.is_none()
                    {
                        input.transport_queued = Some(queued_at);
                        input.transport_sent = Some(sent_at);
                    }
                }
            }
            ClientEvent::PluginGraphicsChanged {
                pane,
                sequence,
                terminal_input_sequence,
                ready_at,
            } => {
                let mut failure = None;
                if let Some(renderer) = self.renderer.as_mut() {
                    match renderer.refresh_plugin_graphics(pane, sequence) {
                        Ok(change) => {
                            for input in &mut self.pending_plugin_inputs {
                                if input.pane == pane
                                    && input.snapshot_dispatched.is_some()
                                    && input.trace_sequence.is_some()
                                    && input.trace_sequence <= terminal_input_sequence
                                {
                                    input.graphics_ready = Some(ready_at);
                                    input.ready = true;
                                }
                            }
                            if let Some(benchmark) = self.plugin_scroll_benchmark.as_mut()
                                && benchmark.pane == pane
                            {
                                benchmark.record_scene(Instant::now());
                            } else if std::env::var("BROOD_TERMINAL_SCROLL_BENCH_AUTO").as_deref()
                                == Ok("1")
                            {
                                let now = Instant::now();
                                if let Some((previous_pane, previous)) =
                                    self.plugin_scroll_auto_last_scene
                                    && previous_pane == pane
                                    && now.duration_since(previous) <= Duration::from_millis(10)
                                {
                                    let mut benchmark = PluginScrollBenchmark::new(pane);
                                    benchmark.record_scene(previous);
                                    benchmark.record_scene(now);
                                    self.plugin_scroll_benchmark = Some(benchmark);
                                    PLUGIN_SCROLL_BENCH_ACTIVE.store(true, Ordering::Relaxed);
                                }
                                self.plugin_scroll_auto_last_scene = Some((pane, now));
                            }
                            if let Some(change) = change
                                && let Err(error) = dispatch_focus_change(
                                    self.wasm_runtime.as_ref(),
                                    self.input_sender.as_mut(),
                                    change,
                                    self.window_focused,
                                )
                            {
                                failure = Some(error);
                            }
                        }
                        Err(error) => failure = Some(error),
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                    if failure.is_none()
                        && let Err(error) = flush_renderer_plugin_resize_events(
                            self.wasm_runtime.as_ref(),
                            renderer,
                        )
                    {
                        failure = Some(error);
                    }
                }
                if let Some(error) = failure {
                    self.fail(event_loop, error);
                }
            }
            ClientEvent::RendererFailed {
                generation,
                message,
            } => {
                if let Err(error) = self.recover_renderer(generation, &message) {
                    self.fail(event_loop, error);
                }
            }
            ClientEvent::BackendCommandCompleted {
                description,
                result,
            } => match result {
                Ok(response) => eprintln!(
                    "presentation backend command completed command={description} response={}",
                    response.trim()
                ),
                Err(error) => {
                    eprintln!("presentation backend command failed command={description}: {error}");
                }
            },
            ClientEvent::BackendSnapshot {
                backend,
                session,
                active_session,
                windows,
            } => {
                let local_panes = windows.iter().map(|window| window.pane).collect::<Vec<_>>();
                let first_local_pane = local_panes.first().copied();
                if !self
                    .backend_registry
                    .replace_panes(backend, None, local_panes)
                {
                    return;
                }
                let windows = windows
                    .into_iter()
                    .map(|mut window| {
                        window.pane = self.backend_registry.view_pane(QualifiedPane {
                            backend,
                            pane: window.pane,
                        });
                        window
                    })
                    .collect::<Vec<_>>();
                if let Some(sender) = self.input_sender.as_mut() {
                    sender.set_pane_routes(self.backend_registry.all_view_routes());
                }
                if let (Some(window), Some(first_local_pane)) = (windows.first(), first_local_pane)
                {
                    debug_assert_eq!(
                        self.backend_registry.backend_pane(window.pane),
                        Some(QualifiedPane {
                            backend,
                            pane: first_local_pane,
                        })
                    );
                }
                let _backend_count = self.backend_registry.backend_count();
                let focus_change = if let Some(renderer) = self.renderer.as_mut() {
                    renderer.select_backend_snapshot(backend, active_session, windows)
                } else {
                    self.pending_backend_snapshots
                        .insert(backend, (active_session, windows));
                    None
                };
                if let Some(change) = focus_change
                    && let Err(error) = dispatch_focus_change(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        change,
                        self.window_focused,
                    )
                {
                    self.fail(event_loop, error);
                    return;
                }
                if let Some(snapshot) = session {
                    self.backend_sessions.insert(backend, snapshot.clone());
                    if let Some(runtime) = self.wasm_runtime.as_mut() {
                        runtime.bind_session_snapshot(&snapshot);
                        if let Err(error) = runtime
                            .broadcast_session_event(&SessionEvent::Snapshot(snapshot.clone()))
                        {
                            self.fail(event_loop, error);
                            return;
                        }
                        runtime.stop_actors_for_closed_sessions(&snapshot);
                    }
                }
                if let Err(error) = self.publish_backend_sessions() {
                    eprintln!("could not publish backend session catalogue: {error:#}");
                }
            }
            ClientEvent::SessionCommandCompleted {
                actor_id,
                completion,
            } => {
                eprintln!(
                    "presentation session command completed actor={} request={} operation={:?} success={} session={:?} message={}",
                    actor_id,
                    completion.request,
                    completion.operation,
                    completion.success,
                    completion.session,
                    completion.message,
                );
                let focus_change = completion
                    .success
                    .then_some(completion.operation)
                    .filter(|operation| {
                        matches!(
                            operation,
                            SessionOperation::Activate | SessionOperation::Create
                        )
                    })
                    .and(completion.session)
                    .and_then(|session| {
                        self.renderer.as_mut().and_then(|renderer| {
                            renderer
                                .select_active_session(BackendId(completion.backend_id), session)
                        })
                    });
                if let Some(change) = focus_change
                    && let Err(error) = dispatch_focus_change(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        change,
                        self.window_focused,
                    )
                {
                    self.fail(event_loop, error);
                    return;
                }
                if let Some(runtime) = self.wasm_runtime.as_ref()
                    && completion.success
                    && completion.operation == SessionOperation::Refresh
                    && let Some(snapshot) = self
                        .backend_sessions
                        .get(&BackendId(completion.backend_id))
                        .cloned()
                    && let Err(error) =
                        runtime.send_session_event(&actor_id, SessionEvent::Snapshot(snapshot))
                {
                    self.fail(event_loop, error);
                    return;
                }
                if let Some(runtime) = self.wasm_runtime.as_ref()
                    && let Err(error) = runtime
                        .send_session_event(&actor_id, SessionEvent::CommandCompleted(completion))
                {
                    self.fail(event_loop, error);
                }
            }
            ClientEvent::ActorHostMessagesReady => {
                if let Err(error) = self
                    .apply_actor_host_messages()
                    .and_then(|()| self.publish_layout_state())
                {
                    self.fail(event_loop, error);
                }
            }
            ClientEvent::BackendActorMessage {
                backend,
                realm,
                sender,
                recipient,
                body,
                received_at,
            } => {
                if !self.backend_registry.observe(backend, None) {
                    return;
                }
                if body.starts_with(b"BTRM")
                    && sender.starts_with("terminal-session")
                    && let Some(pane) = self.wasm_runtime.as_ref().and_then(|runtime| {
                        runtime
                            .pane_actor_ids()
                            .find_map(|(pane, actor)| (actor == recipient).then_some(pane))
                    })
                {
                    let dispatched = Instant::now();
                    let acknowledged = terminal_snapshot_completes_revision(&body)
                        .then(|| terminal_snapshot_input_sequence(&body))
                        .flatten();
                    if let Some(acknowledged) = acknowledged
                        && let Some(plugin_graphics) = self.plugin_graphics.as_ref()
                        && let Err(error) =
                            plugin_graphics.expect_terminal_snapshot(pane, acknowledged)
                    {
                        eprintln!(
                            "could not correlate terminal snapshot with plugin graphics \
                             pane={pane}: {error:#}"
                        );
                    }
                    for input in &mut self.pending_plugin_inputs {
                        if input.pane == pane
                            && input.backend == Some(backend)
                            && input.trace_sequence.is_some()
                            && input.trace_sequence <= acknowledged
                            && input.transport_received.is_none()
                        {
                            input.transport_received = Some(received_at);
                            input.snapshot_dispatched = Some(dispatched);
                        }
                    }
                }
                if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some()
                    && !PLUGIN_SCROLL_BENCH_ACTIVE.load(Ordering::Relaxed)
                {
                    if body.starts_with(b"BTRM") {
                        eprintln!(
                            "backend actor message backend={} realm={realm:?} sender={sender} recipient={recipient} terminal_snapshot_bytes={}",
                            backend.0,
                            body.len()
                        );
                    } else {
                        eprintln!(
                            "backend actor message backend={} realm={realm:?} sender={sender} recipient={recipient} body={}",
                            backend.0,
                            String::from_utf8_lossy(&body)
                        );
                    }
                }
                if let Some(runtime) = self.wasm_runtime.as_ref()
                    && let Err(error) = runtime
                        .send_backend_actor_message(backend, realm, &recipient, &sender, body)
                {
                    eprintln!(
                        "dropped backend actor message for unavailable presentation actor \
                         {recipient:?}: {error:#}"
                    );
                }
            }
            event => {
                if let Err(error) = self.accept_frame(event) {
                    self.fail(event_loop, error);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.drain_wasm_events(event_loop) {
            return;
        }
        if let Some(wayland_drop) = self.wayland_drop.as_mut()
            && let Err(error) = wayland_drop.dispatch_pending()
        {
            self.fail(event_loop, error);
            return;
        }
        if let Err(error) = self.settle_window_resize() {
            self.fail(event_loop, error);
            return;
        }
        self.sync_default_backend_target();
        if let Err(error) = self.publish_layout_state() {
            self.fail(event_loop, error);
            return;
        }
        if let Some(renderer) = self.renderer.as_mut()
            && let Err(error) =
                flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)
        {
            self.fail(event_loop, error);
            return;
        }
        let input_backpressured = self.input_sender.as_mut().is_some_and(|lane| {
            lane.flush_deferred();
            lane.has_deferred()
        });
        let actor_message_backpressured = self.actor_message_sender.as_mut().is_some_and(|lane| {
            lane.flush_deferred();
            lane.has_deferred()
        });
        // A full actor lane keeps the latest position for the next Winit turn.
        // Motion is state, so intermediate samples are disposable.
        let _motion_result = flush_presentation_pointer_motion(
            self.wasm_runtime.as_ref(),
            &mut self.pointer_routing.pending_plugin_motion,
        );
        let auto_scroll_benchmark =
            std::env::var("BROOD_TERMINAL_SCROLL_BENCH_AUTO").as_deref() == Ok("1");
        if auto_scroll_benchmark
            && self
                .plugin_scroll_benchmark
                .as_ref()
                .is_some_and(|benchmark| {
                    benchmark.input_events == 0
                        && benchmark
                            .last_scene
                            .is_some_and(|last| last.elapsed() >= Duration::from_millis(80))
                })
        {
            if let Some(benchmark) = self.plugin_scroll_benchmark.take()
                && benchmark.scene_count() >= 30
            {
                eprintln!(
                    "plugin helix scroll diagnostics {}",
                    benchmark.diagnostics()
                );
            }
            PLUGIN_SCROLL_BENCH_ACTIVE.store(false, Ordering::Relaxed);
        }
        let scroll_benchmark_deadline = if auto_scroll_benchmark {
            self.plugin_scroll_benchmark
                .as_ref()
                .filter(|benchmark| benchmark.input_events == 0)
                .and_then(|benchmark| benchmark.last_scene)
                .map(|last| last + Duration::from_millis(80))
        } else {
            None
        };
        if input_backpressured || actor_message_backpressured {
            // The shared-memory consumer already has a wakeup for the records
            // which filled its ring. Retry promptly without busy-spinning a
            // CPU while the consumer is descheduled or slow to free a slot.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(1),
            ));
        } else if let Some(resize) = self.window_resize.as_ref() {
            let actor_poll = Instant::now() + Duration::from_millis(50);
            let mut deadline = if self
                .wasm_runtime
                .as_ref()
                .is_some_and(PresentationWasmRuntime::requires_periodic_event_poll)
            {
                resize.settle_at.min(actor_poll)
            } else {
                resize.settle_at
            };
            if let Some(scroll_benchmark_deadline) = scroll_benchmark_deadline {
                deadline = deadline.min(scroll_benchmark_deadline);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else if self
            .wasm_runtime
            .as_ref()
            .is_some_and(PresentationWasmRuntime::requires_periodic_event_poll)
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(50),
            ));
        } else if let Some(deadline) = scroll_benchmark_deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.drop_frontend_wayland_state();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if renderer.window.id() != window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                let now = Instant::now();
                let previous_size = (renderer.config.width, renderer.config.height);
                if self.window_resize.is_none() && previous_size == (size.width, size.height) {
                    return;
                }
                if let Some(transaction) = self.window_resize.as_mut() {
                    transaction.observe(now, (size.width, size.height));
                } else {
                    self.window_resize = Some(WindowResizeTransaction::new(
                        now,
                        previous_size,
                        (size.width, size.height),
                    ));
                }
                let focus_change = renderer.resize_window_live(size.width, size.height);
                if let Some(change) = focus_change
                    && let Err(error) = dispatch_focus_change(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        change,
                        self.window_focused,
                    )
                {
                    self.fail(event_loop, error);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some((position.x, position.y));
                route_pointer_motion(
                    renderer,
                    self.cursor_position,
                    &mut self.pointer_grab,
                    self.wasm_runtime.as_ref(),
                    self.input_sender.as_mut(),
                    &mut self.pointer_routing,
                );
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_position = None;
                self.pointer_routing.pending_plugin_motion = None;
                send_plugin_pointer_leave(
                    self.wasm_runtime.as_ref(),
                    &mut self.pointer_routing.plugin_target,
                );
                send_managed_pointer_leave(
                    self.input_sender.as_mut(),
                    &mut self.pointer_routing.managed_target,
                );
            }
            WindowEvent::DroppedFile(path) => {
                deliver_dropped_image(
                    renderer,
                    self.wasm_runtime.as_ref(),
                    self.cursor_position,
                    &path,
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button_code) = linux_button_code(button) else {
                    return;
                };
                let pressed = state == ElementState::Pressed;
                if !pressed
                    && self
                        .pointer_grab
                        .as_ref()
                        .is_none_or(|grab| !grab.buttons.contains(&button_code))
                {
                    return;
                }
                let Some(target) = pointer_target_for_cursor(
                    renderer,
                    self.cursor_position,
                    self.pointer_grab.as_ref(),
                ) else {
                    return;
                };
                eprintln!(
                    "presentation pointer button target={target:?} pressed={pressed} client_grab_active={}",
                    self.pointer_grab.is_some()
                );
                if let Some(grab) = self.pointer_grab.as_mut() {
                    grab.target = target;
                    if pressed {
                        grab.press(button_code);
                    }
                } else if pressed {
                    self.pointer_grab = Some(PointerGrab::new(target, button_code));
                }

                if pressed && let Some(change) = renderer.focus_pane(target.pane()) {
                    if let Err(error) = dispatch_focus_change(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        change,
                        self.window_focused,
                    ) {
                        eprintln!("dropped pane focus event: {error:#}");
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                }

                send_pointer_button_to_target(
                    self.wasm_runtime.as_ref(),
                    self.input_sender.as_mut(),
                    &mut self.pointer_routing,
                    target,
                    PointerButtonState {
                        button: button_code,
                        pressed,
                        modifiers: modifier_bits(self.modifiers),
                    },
                );

                let grab_ended = !pressed
                    && self
                        .pointer_grab
                        .as_mut()
                        .is_some_and(|grab| grab.release(button_code));
                if grab_ended {
                    eprintln!("presentation pointer grab ended target={target:?}");
                    self.pointer_grab = None;
                    route_pointer_motion(
                        renderer,
                        self.cursor_position,
                        &mut self.pointer_grab,
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        &mut self.pointer_routing,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(target) = pointer_target_for_cursor(
                    renderer,
                    self.cursor_position,
                    self.pointer_grab.as_ref(),
                ) else {
                    return;
                };
                eprintln!(
                    "presentation pointer scroll target={target:?} client_grab_active={}",
                    self.pointer_grab.is_some()
                );
                if let Some(grab) = self.pointer_grab.as_mut() {
                    grab.target = target;
                }
                match target {
                    PanePointerTarget::Plugin { pane, x, y } => {
                        let scale = renderer.window.scale_factor() as f32;
                        let (horizontal, vertical) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x * 15.0, y * 15.0),
                            MouseScrollDelta::PixelDelta(position) => {
                                (position.x as f32, position.y as f32)
                            }
                        };
                        send_managed_pointer_leave(
                            self.input_sender.as_mut(),
                            &mut self.pointer_routing.managed_target,
                        );
                        set_plugin_pointer_target(
                            self.wasm_runtime.as_ref(),
                            &mut self.pointer_routing.plugin_target,
                            pane,
                        );
                        if let Err(error) = send_presentation_event(
                            self.wasm_runtime.as_ref(),
                            PresentationEvent::PointerScroll {
                                pane,
                                x,
                                y,
                                horizontal: horizontal / scale,
                                vertical: vertical / scale,
                                modifiers: modifier_bits(self.modifiers),
                            },
                        ) {
                            eprintln!("dropped plugin pointer scroll: {error:#}");
                        }
                    }
                    PanePointerTarget::Managed { pane, x, y } => {
                        self.pointer_routing.pending_plugin_motion = None;
                        send_plugin_pointer_leave(
                            self.wasm_runtime.as_ref(),
                            &mut self.pointer_routing.plugin_target,
                        );
                        if let Some(sender) = self.input_sender.as_mut() {
                            let sent = match delta {
                                MouseScrollDelta::LineDelta(horizontal, vertical) => send_input(
                                    sender,
                                    ClientMessage::Input(InputEvent::PointerWheel {
                                        position: PanePosition { pane, x, y },
                                        horizontal_v120: wheel_v120(horizontal),
                                        vertical_v120: wheel_v120(vertical),
                                    }),
                                ),
                                MouseScrollDelta::PixelDelta(position) => {
                                    let scale = renderer.window.scale_factor().max(f64::EPSILON);
                                    send_input(
                                        sender,
                                        ClientMessage::Input(InputEvent::PointerAxis {
                                            position: PanePosition { pane, x, y },
                                            horizontal: -position.x / scale,
                                            vertical: -position.y / scale,
                                        }),
                                    )
                                }
                            };
                            if sent {
                                self.pointer_routing.managed_target = Some(pane);
                            }
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                if let Some(sender) = self.input_sender.as_mut() {
                    let _sent = send_input(
                        sender,
                        ClientMessage::Input(InputEvent::PresentationFocus { focused }),
                    );
                }
                if !focused {
                    if let Some(sender) = self.input_sender.as_mut() {
                        send_keyboard_reset(sender);
                    }
                    cancel_pointer_grab(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        &mut self.pointer_routing,
                        &mut self.pointer_grab,
                        modifier_bits(self.modifiers),
                    );
                    self.cursor_position = None;
                    self.pointer_routing.pending_plugin_motion = None;
                    send_plugin_pointer_leave(
                        self.wasm_runtime.as_ref(),
                        &mut self.pointer_routing.plugin_target,
                    );
                    send_managed_pointer_leave(
                        self.input_sender.as_mut(),
                        &mut self.pointer_routing.managed_target,
                    );
                }
                if let Some(pane) = renderer.focused_plugin_pane()
                    && let Err(error) = send_presentation_event(
                        self.wasm_runtime.as_ref(),
                        PresentationEvent::PaneFocused { pane, focused },
                    )
                {
                    eprintln!("dropped plugin outer-window focus event: {error:#}");
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let text_input =
                    plugin_text_input(&event.logical_key, event.text.as_deref(), self.modifiers);
                let logical_key = plugin_logical_key(&event.logical_key);
                if std::env::var_os("BROOD_TERMINAL_SCROLL_BENCH").is_some()
                    && matches!(logical_key.as_str(), "F11" | "F12")
                    && let Some(pane) = renderer.focused_plugin_pane()
                {
                    if event.state == ElementState::Pressed {
                        if logical_key == "F12" {
                            self.plugin_scroll_benchmark = Some(PluginScrollBenchmark::new(pane));
                            PLUGIN_SCROLL_BENCH_ACTIVE.store(true, Ordering::Relaxed);
                        } else {
                            if let Some(benchmark) = self.plugin_scroll_benchmark.take() {
                                eprintln!(
                                    "plugin helix scroll diagnostics {}",
                                    benchmark.diagnostics()
                                );
                            }
                            PLUGIN_SCROLL_BENCH_ACTIVE.store(false, Ordering::Relaxed);
                        }
                    }
                    return;
                }
                if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some()
                    && !PLUGIN_SCROLL_BENCH_ACTIVE.load(Ordering::Relaxed)
                {
                    eprintln!(
                        "plugin keyboard input logical={logical_key:?} physical={:?} \
                         state={:?} synthetic={is_synthetic}",
                        event.physical_key, event.state
                    );
                }
                let policy_evaluation =
                    event.physical_key.to_scancode().and_then(|scancode| {
                        let mut snapshot = renderer.key_policy_snapshot();
                        if let Some(runtime) = self.wasm_runtime.as_ref() {
                            snapshot.app_panes.extend(runtime.pane_actor_ids().map(
                                |(pane, actor)| KeyPolicyAppPane {
                                    pane,
                                    app_id: actor.to_string(),
                                },
                            ));
                        }
                        self.key_policy.as_mut().and_then(|policy| {
                            match policy.evaluate(
                                i32::try_from(scancode.saturating_add(8)).unwrap_or(i32::MAX),
                                match event.state {
                                    ElementState::Pressed => brood_wasm::ABI_KEY_PRESSED,
                                    ElementState::Released => brood_wasm::ABI_KEY_RELEASED,
                                },
                                key_policy_modifier_bits(self.modifiers),
                                event.repeat,
                                &snapshot,
                            ) {
                                Ok(evaluation) => Some(evaluation),
                                Err(error) => {
                                    eprintln!(
                                        "presentation key policy failed; forwarding key: {error:#}"
                                    );
                                    None
                                }
                            }
                        })
                    });

                let mut forward_key = true;
                if let Some(evaluation) = policy_evaluation {
                    forward_key = matches!(
                        evaluation.decision,
                        GlobalKeyDecision::Abstain
                            | GlobalKeyDecision::Forward
                            | GlobalKeyDecision::ForwardWithModifiers
                    );
                    for message in evaluation.actor_messages {
                        if let Some(runtime) = self.wasm_runtime.as_ref() {
                            match runtime.send_key_policy_message(&message.actor, message.payload) {
                                Ok(()) => eprintln!(
                                    "sent key-policy actor message actor={}",
                                    message.actor
                                ),
                                Err(error) => {
                                    eprintln!("dropped key-policy actor message: {error:#}");
                                }
                            }
                        }
                    }
                    for message in evaluation.host_messages {
                        if let Err(error) = apply_key_policy_host_message(
                            renderer,
                            &mut self.input_sender,
                            self.backend_commands.as_ref(),
                            self.wasm_runtime.as_mut(),
                            self.clipboard.as_ref(),
                            message,
                            self.window_focused,
                        ) {
                            eprintln!("rejected key-policy host message: {error:#}");
                        }
                    }
                    flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
                }

                if !forward_key {
                    return;
                }

                let modifiers = modifier_bits(self.modifiers);
                if let Some(pane) = renderer.focused_plugin_pane() {
                    let physical_key = event.physical_key.to_scancode().unwrap_or_default();
                    let sent = send_presentation_event(
                        self.wasm_runtime.as_ref(),
                        PresentationEvent::Key {
                            pane,
                            physical_key,
                            logical_key,
                            pressed: event.state == ElementState::Pressed,
                            repeat: event.repeat,
                            modifiers,
                        },
                    );
                    let delivered = sent.is_ok();
                    let terminal_input = self.wasm_runtime.as_ref().is_some_and(|runtime| {
                        runtime.pane_actor_ids().any(|(actor_pane, actor)| {
                            actor_pane == pane && actor.starts_with("terminal-ui")
                        })
                    });
                    if let Err(error) = sent {
                        eprintln!("dropped plugin key event: {error:#}");
                    } else if event.state == ElementState::Pressed
                        && event.text.as_ref().is_some_and(|text| !text.is_empty())
                        && terminal_input
                    {
                        self.pending_plugin_inputs.push_back(PendingPluginInput {
                            pane,
                            backend: renderer
                                .plugin_pane_realms
                                .get(&pane)
                                .copied()
                                .flatten()
                                .map(|(backend, _session)| backend),
                            trace_sequence: None,
                            started: Instant::now(),
                            started_unix_us: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_or(0, |elapsed| elapsed.as_micros()),
                            transport_queued: None,
                            transport_sent: None,
                            transport_received: None,
                            snapshot_dispatched: None,
                            graphics_ready: None,
                            ready: false,
                        });
                        if self.pending_plugin_inputs.len() > 4_096 {
                            self.pending_plugin_inputs.pop_front();
                        }
                    }
                    if delivered
                        && event.state == ElementState::Pressed
                        && let Some(benchmark) = self.plugin_scroll_benchmark.as_mut()
                        && benchmark.pane == pane
                    {
                        benchmark.record_input(Instant::now());
                    }
                    if event.state == ElementState::Pressed
                        && let Some(text) = text_input.as_deref()
                    {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some()
                            && !PLUGIN_SCROLL_BENCH_ACTIVE.load(Ordering::Relaxed)
                        {
                            eprintln!("plugin text input text={text:?}");
                        }
                        if let Err(error) = send_presentation_event(
                            self.wasm_runtime.as_ref(),
                            PresentationEvent::Text {
                                pane,
                                text: text.to_string(),
                            },
                        ) {
                            eprintln!("dropped plugin text event: {error:#}");
                        }
                    }
                } else if renderer.focused_managed_window().is_some()
                    && let (Some(sender), Some(scancode)) =
                        (self.input_sender.as_mut(), event.physical_key.to_scancode())
                {
                    send_input(
                        sender,
                        ClientMessage::Input(InputEvent::KeyboardKey {
                            keycode: scancode + 8,
                            pressed: event.state == ElementState::Pressed,
                        }),
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(benchmark) = self.plugin_scroll_benchmark.as_mut() {
                    benchmark.record_redraw();
                }
                match renderer.render() {
                    RenderOutcome::Presented {
                        sequence,
                        reconfigure,
                        timings,
                    } => {
                        self.renderer_recoveries = 0;
                        if let Some(benchmark) = self.plugin_scroll_benchmark.as_mut() {
                            benchmark.record_present(Instant::now(), timings);
                        }
                        let now = Instant::now();
                        let mut completed_inputs = Vec::new();
                        self.pending_plugin_inputs.retain(|input| {
                            if input.ready {
                                completed_inputs.push((
                                    now.duration_since(input.started).as_micros(),
                                    input.backend,
                                    input.pane,
                                    input.trace_sequence,
                                    input.started,
                                    input.started_unix_us,
                                    input.transport_queued,
                                    input.transport_sent,
                                    input.transport_received,
                                    input.snapshot_dispatched,
                                    input.graphics_ready,
                                ));
                                false
                            } else {
                                true
                            }
                        });
                        for (
                            elapsed,
                            backend,
                            pane,
                            trace_sequence,
                            started,
                            started_unix_us,
                            transport_queued,
                            transport_sent,
                            transport_received,
                            snapshot_dispatched,
                            graphics_ready,
                        ) in completed_inputs
                        {
                            self.plugin_key_to_present_samples =
                                self.plugin_key_to_present_samples.saturating_add(1);
                            self.plugin_last_key_to_present_us = elapsed;
                            self.plugin_max_key_to_present_us =
                                self.plugin_max_key_to_present_us.max(elapsed);
                            self.plugin_key_to_present_latencies.push_back(elapsed);
                            if self.plugin_key_to_present_latencies.len() > 4_096 {
                                self.plugin_key_to_present_latencies.pop_front();
                            }
                            if let Some(backend) = backend {
                                let samples = self
                                    .plugin_key_to_present_by_backend
                                    .entry(backend)
                                    .or_default();
                                push_bounded(samples, elapsed);
                                if let (
                                    Some(transport_received),
                                    Some(snapshot_dispatched),
                                    Some(graphics_ready),
                                ) = (transport_received, snapshot_dispatched, graphics_ready)
                                {
                                    self.plugin_latency_stages_by_backend
                                        .entry(backend)
                                        .or_default()
                                        .record(
                                            started,
                                            transport_queued,
                                            transport_sent,
                                            transport_received,
                                            snapshot_dispatched,
                                            graphics_ready,
                                            now,
                                        );
                                    if let Some(trace_sequence) = trace_sequence
                                        && let (Some(transport_queued), Some(transport_sent)) =
                                            (transport_queued, transport_sent)
                                    {
                                        let traces = self
                                            .plugin_input_traces_by_backend
                                            .entry(backend)
                                            .or_default();
                                        traces.push_back(serde_json::json!({
                                            "sequence": trace_sequence,
                                            "pane": pane,
                                            "started_unix_us": started_unix_us,
                                            "total_us": elapsed,
                                            "input_to_transport_queue_us": transport_queued
                                                .duration_since(started)
                                                .as_micros(),
                                            "transport_send_queue_us": transport_sent
                                                .duration_since(transport_queued)
                                                .as_micros(),
                                            "remote_round_trip_us": transport_received
                                                .duration_since(transport_sent)
                                                .as_micros(),
                                            "client_dispatch_us": snapshot_dispatched
                                                .duration_since(transport_received)
                                                .as_micros(),
                                            "plugin_render_us": graphics_ready
                                                .duration_since(snapshot_dispatched)
                                                .as_micros(),
                                            "presentation_us": now
                                                .duration_since(graphics_ready)
                                                .as_micros(),
                                        }));
                                        if traces.len() > 4_096 {
                                            traces.pop_front();
                                        }
                                    }
                                }
                            }
                        }
                        if reconfigure {
                            renderer.reconfigure();
                        }
                        if sequence.is_some()
                            && let Some(sender) = self.input_sender.as_mut()
                        {
                            for (backend, sequence) in renderer.frame_sequences() {
                                if self.last_presented_sequences.get(&backend) == Some(&sequence) {
                                    continue;
                                }
                                if sender.send_to(
                                    backend,
                                    ClientMessage::Lifecycle(ClientLifecycle::FramePresented {
                                        sequence,
                                    }),
                                ) {
                                    self.last_presented_sequences.insert(backend, sequence);
                                } else {
                                    renderer.window.request_redraw();
                                }
                            }
                        }
                    }
                    RenderOutcome::Skipped => {
                        if let Some(benchmark) = self.plugin_scroll_benchmark.as_mut() {
                            benchmark.record_skipped();
                        }
                    }
                    RenderOutcome::Reconfigure => {
                        if let Some(benchmark) = self.plugin_scroll_benchmark.as_mut() {
                            benchmark.record_reconfigure();
                        }
                        renderer.reconfigure();
                    }
                    RenderOutcome::Failed(message) => {
                        let generation = renderer.generation;
                        if let Err(error) = self.recover_renderer(generation, message) {
                            self.fail(event_loop, error);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn plugin_logical_key(key: &Key) -> String {
    match key {
        Key::Character(character) => character.to_string(),
        Key::Named(named) => match named {
            NamedKey::Enter => "Enter",
            NamedKey::Escape => "Escape",
            NamedKey::Backspace => "Backspace",
            NamedKey::Tab => "Tab",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PageUp",
            NamedKey::PageDown => "PageDown",
            NamedKey::Insert => "Insert",
            NamedKey::Delete => "Delete",
            NamedKey::ArrowUp => "ArrowUp",
            NamedKey::ArrowDown => "ArrowDown",
            NamedKey::ArrowLeft => "ArrowLeft",
            NamedKey::ArrowRight => "ArrowRight",
            NamedKey::F1 => "F1",
            NamedKey::F2 => "F2",
            NamedKey::F3 => "F3",
            NamedKey::F4 => "F4",
            NamedKey::F5 => "F5",
            NamedKey::F6 => "F6",
            NamedKey::F7 => "F7",
            NamedKey::F8 => "F8",
            NamedKey::F9 => "F9",
            NamedKey::F10 => "F10",
            NamedKey::F11 => "F11",
            NamedKey::F12 => "F12",
            _ => "",
        }
        .to_string(),
        Key::Dead(character) => {
            character.map_or_else(String::new, |character| character.to_string())
        }
        Key::Unidentified(_) => String::new(),
    }
}

fn plugin_text_input(
    logical_key: &Key,
    event_text: Option<&str>,
    modifiers: ModifiersState,
) -> Option<String> {
    if let Some(text) = event_text.filter(|text| !text.is_empty()) {
        return Some(text.to_string());
    }
    if modifiers.control_key() || modifiers.alt_key() || modifiers.super_key() {
        return None;
    }
    let Key::Character(character) = logical_key else {
        return None;
    };
    (!character.is_empty() && !character.chars().any(char::is_control))
        .then(|| character.to_string())
}

struct Renderer {
    generation: u64,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    frame_bind_group_layout: wgpu::BindGroupLayout,
    frame_sampler: wgpu::Sampler,
    frame_pipeline: wgpu::RenderPipeline,
    placeholder_pipeline: wgpu::RenderPipeline,
    plugin_canvas: GpuCanvasRenderer,
    imported_frames: HashMap<u64, ImportedFrame>,
    frame_keys: BTreeMap<(BackendId, u64), u64>,
    next_frame_key: u64,
    available_windows: Vec<AvailableWindow>,
    active_session: u64,
    active_backend: Option<BackendId>,
    active_sessions: BTreeMap<BackendId, u64>,
    current_windows: Vec<SceneWindow>,
    current_sequence: Option<u64>,
    current_sequences: BTreeMap<BackendId, u64>,
    plugin_graphics: PluginGraphicsBridge,
    plugin_panes: BTreeMap<u64, PluginPaneTexture>,
    plugin_pane_realms: BTreeMap<u64, Option<(BackendId, u64)>>,
    focused_pane: Option<FocusedPane>,
    pending_focus_pane: Option<u64>,
    accepted_layout: ResolvedLayout,
    layout_source: LayoutSource,
    layout_state_dirty: bool,
    sent_managed_sizes: HashMap<u64, (u32, u32)>,
    sent_plugin_sizes: HashMap<u64, PluginPaneSize>,
    window_resize_in_progress: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusedPane {
    Plugin(u64),
    Managed(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PanePointerTarget {
    Plugin { pane: u64, x: f32, y: f32 },
    Managed { pane: u64, x: f64, y: f64 },
}

impl PanePointerTarget {
    const fn pane(self) -> u64 {
        match self {
            Self::Plugin { pane, .. } | Self::Managed { pane, .. } => pane,
        }
    }
}

#[derive(Debug)]
struct PointerGrab {
    target: PanePointerTarget,
    buttons: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerButtonState {
    button: u32,
    pressed: bool,
    modifiers: u32,
}

impl PointerGrab {
    fn new(target: PanePointerTarget, button: u32) -> Self {
        Self {
            target,
            buttons: vec![button],
        }
    }

    fn press(&mut self, button: u32) {
        if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
    }

    fn release(&mut self, button: u32) -> bool {
        self.buttons.retain(|pressed| *pressed != button);
        self.buttons.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FocusChange {
    plugin_blur: Option<u64>,
    plugin_focus: Option<u64>,
    managed_window: Option<u64>,
    reset_keyboard: bool,
}

struct SceneWindow {
    backend: BackendId,
    window_id: u64,
    resource_id: u64,
    source_rect: [i32; 4],
    rect: [i32; 4],
    active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct AvailableWindow {
    backend: BackendId,
    pane: u64,
    session: u64,
    app_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
struct LayoutPaneRealm {
    pane: u64,
    backend: BackendId,
    session: u64,
}

#[derive(serde::Serialize)]
struct LayoutStateMessage {
    #[serde(rename = "type")]
    message_type: &'static str,
    revision: u64,
    lease: brood_wasm::LayoutLeaseToken,
    visible_panes: Vec<u64>,
    active_session: u64,
    available_windows: Vec<AvailableWindow>,
    pane_realms: Vec<LayoutPaneRealm>,
    focused_pane: Option<u64>,
    pane_app_ids: Vec<KeyPolicyAppPane>,
    size: LayoutStateSize,
    rest: bool,
}

#[derive(serde::Serialize)]
struct LayoutStateSize {
    w: i32,
    h: i32,
}

fn layout_pane_realms(
    available_windows: &[AvailableWindow],
    plugin_pane_realms: &BTreeMap<u64, Option<(BackendId, u64)>>,
    owned_panes: &[u64],
) -> Vec<LayoutPaneRealm> {
    let mut pane_realms = available_windows
        .iter()
        .filter(|window| owned_panes.contains(&window.pane))
        .map(|window| LayoutPaneRealm {
            pane: window.pane,
            backend: window.backend,
            session: window.session,
        })
        .chain(plugin_pane_realms.iter().filter_map(|(pane, realm)| {
            let (backend, session) = (*realm)?;
            owned_panes.contains(pane).then_some(LayoutPaneRealm {
                pane: *pane,
                backend,
                session,
            })
        }))
        .collect::<Vec<_>>();
    pane_realms.sort_by_key(|realm| realm.pane);
    pane_realms.dedup_by_key(|realm| realm.pane);
    pane_realms
}

struct ImportedFrame {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    source: ImportedFrameSource,
}

enum ImportedFrameSource {
    Dmabuf,
    SharedMemory(brood_ipc::SharedImageConsumer),
    Network,
}

struct PluginPaneTexture {
    scene: GpuCanvasScene,
    rect: [i32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PluginPaneSize {
    width: f32,
    height: f32,
    scale: f32,
}

fn send_presentation_configuration(lane: &mut InputLane, renderer: &Renderer) {
    let size = renderer.window.inner_size();
    let refresh_millihz = renderer
        .window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .unwrap_or(60_000);
    send_input(
        lane,
        ClientMessage::Input(InputEvent::PresentationSize {
            width: size.width.max(1),
            height: size.height.max(1),
            refresh_millihz,
        }),
    );
}

fn flush_renderer_managed_resize_requests(lane: Option<&mut InputLane>, renderer: &mut Renderer) {
    let Some(lane) = lane else {
        return;
    };
    for (window, width, height) in renderer.pending_managed_resize_requests() {
        if send_input(
            lane,
            ClientMessage::Input(InputEvent::WindowResize {
                pane: window,
                width,
                height,
            }),
        ) {
            renderer.mark_managed_resize_sent(window, width, height);
            eprintln!(
                "requested managed window resize window_id={window} logical_size={width}x{height}"
            );
        }
    }
}

fn flush_renderer_plugin_resize_events(
    runtime: Option<&PresentationWasmRuntime>,
    renderer: &mut Renderer,
) -> Result<()> {
    for (pane, size) in renderer.pending_plugin_resize_events() {
        send_presentation_event(
            runtime,
            PresentationEvent::PaneResized {
                pane,
                width: size.width,
                height: size.height,
                scale: size.scale,
            },
        )?;
        renderer.mark_plugin_resize_sent(pane, size);
        eprintln!(
            "sent plugin pane resize pane={pane} logical_size={:.0}x{:.0} scale={:.2}",
            size.width, size.height, size.scale
        );
    }
    Ok(())
}

fn send_input(lane: &mut InputLane, message: ClientMessage) -> bool {
    if matches!(
        message,
        ClientMessage::Lifecycle(ClientLifecycle::Disconnect)
            | ClientMessage::Input(
                InputEvent::KeyboardReset
                    | InputEvent::PresentationSize { .. }
                    | InputEvent::PresentationFocus { .. }
            )
    ) {
        lane.broadcast(message)
    } else {
        lane.send(message)
    }
}

fn translate_input_panes(message: &mut ClientMessage, routes: &BTreeMap<u64, u64>) {
    let ClientMessage::Input(input) = message else {
        return;
    };
    let map = |pane: &mut u64| {
        if let Some(backend_pane) = routes.get(pane) {
            *pane = *backend_pane;
        }
    };
    match input {
        InputEvent::WindowFocus { pane } | InputEvent::WindowResize { pane, .. } => map(pane),
        InputEvent::PointerMotion { position }
        | InputEvent::PointerButton { position, .. }
        | InputEvent::PointerAxis { position, .. }
        | InputEvent::PointerWheel { position, .. } => map(&mut position.pane),
        InputEvent::KeyboardKey { .. }
        | InputEvent::KeyboardReset
        | InputEvent::PresentationSize { .. }
        | InputEvent::PresentationFocus { .. }
        | InputEvent::PointerLeave => {}
    }
}

fn send_window_focus(lane: &mut InputLane, window_id: u64) {
    let _sent = send_input(
        lane,
        ClientMessage::Input(InputEvent::WindowFocus { pane: window_id }),
    );
}

fn send_keyboard_reset(lane: &mut InputLane) {
    let _sent = send_input(lane, ClientMessage::Input(InputEvent::KeyboardReset));
}

fn send_presentation_events(
    runtime: Option<&PresentationWasmRuntime>,
    mut create: impl FnMut(u64) -> PresentationEvent,
) -> Result<()> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    let panes = runtime.pane_ids().collect::<Vec<_>>();
    for pane in panes {
        runtime.send_presentation_event(create(pane))?;
    }
    Ok(())
}

fn send_presentation_event(
    runtime: Option<&PresentationWasmRuntime>,
    event: PresentationEvent,
) -> Result<()> {
    let Some(runtime) = runtime else {
        return Ok(());
    };
    runtime.send_presentation_event(event)
}

fn dispatch_focus_change(
    runtime: Option<&PresentationWasmRuntime>,
    mut input_sender: Option<&mut InputLane>,
    change: FocusChange,
    window_focused: bool,
) -> Result<()> {
    if change.reset_keyboard
        && let Some(sender) = input_sender.as_mut()
    {
        send_keyboard_reset(sender);
    }
    if let Some(pane) = change.plugin_blur {
        send_presentation_event(
            runtime,
            PresentationEvent::PaneFocused {
                pane,
                focused: false,
            },
        )?;
        eprintln!("focused away from plugin pane={pane}");
    }
    if let Some(pane) = change.plugin_focus {
        send_presentation_event(
            runtime,
            PresentationEvent::PaneFocused {
                pane,
                focused: window_focused,
            },
        )?;
        eprintln!("focused local plugin pane={pane}");
    }
    if let Some(window_id) = change.managed_window {
        if let Some(sender) = input_sender.as_mut() {
            send_window_focus(sender, window_id);
        }
        eprintln!("focused backend-managed window={window_id}");
    }
    Ok(())
}

fn apply_key_policy_host_message(
    renderer: &mut Renderer,
    input_sender: &mut Option<InputLane>,
    backend_commands: Option<&BackendCommandLane>,
    mut runtime: Option<&mut PresentationWasmRuntime>,
    clipboard: Option<&ClipboardBridge>,
    message: HostMessage,
    window_focused: bool,
) -> Result<()> {
    let focus_change = match message {
        HostMessage::LayoutAction { action } => {
            let parsed = action
                .parse::<LayoutAction>()
                .map_err(|error| anyhow!("parse key-policy layout action: {error}"))?;
            if parsed == LayoutAction::CloseActivePane {
                if let Some(pane) = renderer.focused_managed_window() {
                    queue_backend_command(
                        backend_commands,
                        format!("close-pane-{pane}"),
                        vec!["close-pane".to_string(), pane.to_string()],
                    )?;
                } else if let Some(pane) = renderer.focused_plugin_pane() {
                    let closed = if let Some(runtime) = runtime.as_deref_mut() {
                        runtime.close_pane(pane)?
                    } else {
                        false
                    };
                    eprintln!("key-policy close local pane={pane} changed={closed}");
                } else {
                    eprintln!("key-policy close ignored without a focused pane");
                }
                None
            } else {
                return Err(anyhow!(
                    "layout action {action:?} must be addressed to a layout actor"
                ));
            }
        }
        HostMessage::FocusPane { pane } => {
            let inventory = renderer.pane_inventory();
            let focused = renderer.focused_pane_id();
            let change = renderer.focus_pane(pane);
            let deferred = renderer.pending_focus_pane == Some(pane);
            eprintln!(
                "presentation key policy requested pane focus pane={pane} focused={focused:?} \
                 inventory={inventory:?} changed={} deferred={deferred}",
                change.is_some(),
            );
            change
        }
        HostMessage::SpawnApp { argv } => {
            queue_spawn_command(backend_commands, argv)?;
            None
        }
        HostMessage::FocusOrSpawnApp { app_id, argv } => {
            if let Some(pane) = renderer.pane_for_app_id(&app_id) {
                renderer.focus_pane(pane)
            } else {
                queue_spawn_command(backend_commands, argv)?;
                None
            }
        }
        HostMessage::SwitchActiveSessionNext => {
            queue_backend_command(
                backend_commands,
                "switch-active-session-next",
                vec!["surface-session".to_string(), "next".to_string()],
            )?;
            None
        }
        HostMessage::CreateAndAttachSession => {
            queue_backend_command(
                backend_commands,
                "create-and-attach-session",
                vec!["surface-session".to_string(), "new".to_string()],
            )?;
            None
        }
        HostMessage::ToggleShortcutPassthrough
        | HostMessage::SetShortcutPassthrough { .. }
        | HostMessage::SetKeybinding { .. }
        | HostMessage::BackendSession { .. }
        | HostMessage::ConnectRemoteBackend { .. }
        | HostMessage::DeploySshBackend { .. }
        | HostMessage::DeployContainerBackend { .. }
        | HostMessage::RequestBackendCatalogue
        | HostMessage::CreateLocalBackend => None,
        HostMessage::SetClipboard { text } => {
            clipboard
                .context("presentation clipboard service is unavailable")?
                .set_text(text);
            None
        }
        HostMessage::RequestClipboardPaste { pane } => {
            anyhow::ensure!(
                renderer.focused_plugin_pane() == Some(pane),
                "clipboard paste target is not the focused plugin pane"
            );
            if let Some(content) = clipboard.and_then(ClipboardBridge::content) {
                match content {
                    ClipboardContent::Text(text) => send_presentation_event(
                        runtime.as_deref(),
                        PresentationEvent::Paste { pane, text },
                    )?,
                    ClipboardContent::Image { mime_type, bytes } => {
                        anyhow::ensure!(
                            bytes.len() <= MAX_AGENT_IMAGE_BYTES,
                            "clipboard image exceeds the {MAX_AGENT_IMAGE_BYTES} byte attachment limit"
                        );
                        let detected = image_mime_type(&bytes)
                            .context("clipboard image has an unsupported or invalid format")?;
                        anyhow::ensure!(
                            detected == mime_type,
                            "clipboard image MIME type {mime_type:?} does not match {detected:?} bytes"
                        );
                        let preview = decode_agent_image_preview(&bytes)
                            .context("create clipboard image preview")?;
                        let byte_count = bytes.len();
                        eprintln!(
                            "delivering clipboard image pane={pane} mime={mime_type} bytes={byte_count}"
                        );
                        send_presentation_event(
                            runtime.as_deref(),
                            PresentationEvent::Image {
                                pane,
                                name: format!("clipboard.{}", image_extension(detected)),
                                mime_type,
                                bytes,
                                preview_width: preview.width,
                                preview_height: preview.height,
                                preview_rgba8: preview.rgba8,
                            },
                        )?;
                    }
                }
            }
            None
        }
        HostMessage::SetResolvedLayout { .. }
        | HostMessage::ClaimRootLayout
        | HostMessage::DelegateLayoutRest { .. } => {
            return Err(anyhow!(
                "key policy cannot mutate layout authority directly; it must message a layout actor"
            ));
        }
    };

    if let Some(change) = focus_change {
        dispatch_focus_change(
            runtime.as_deref(),
            input_sender.as_mut(),
            change,
            window_focused,
        )?;
    }
    Ok(())
}

fn deliver_dropped_image(
    renderer: &Renderer,
    runtime: Option<&PresentationWasmRuntime>,
    position: Option<(f64, f64)>,
    path: &std::path::Path,
) {
    let pane = position
        .and_then(|(x, y)| renderer.pointer_target_for_window_point(x, y))
        .and_then(|target| match target {
            PanePointerTarget::Plugin { pane, .. } => Some(pane),
            PanePointerTarget::Managed { .. } => None,
        })
        .or_else(|| renderer.focused_plugin_pane());
    let Some(pane) = pane else {
        eprintln!(
            "ignored dropped file outside a plugin pane path={}",
            path.display()
        );
        return;
    };
    match read_image_attachment(path) {
        Ok((name, mime_type, bytes, preview)) => {
            let byte_count = bytes.len();
            if let Err(error) = send_presentation_event(
                runtime,
                PresentationEvent::Image {
                    pane,
                    name: name.clone(),
                    mime_type: mime_type.clone(),
                    bytes,
                    preview_width: preview.width,
                    preview_height: preview.height,
                    preview_rgba8: preview.rgba8,
                },
            ) {
                eprintln!("dropped image event was rejected: {error:#}");
            } else {
                eprintln!(
                    "delivered dropped image pane={pane} name={name:?} mime={mime_type} bytes={byte_count}"
                );
            }
        }
        Err(error) => {
            eprintln!("ignored dropped file {}: {error:#}", path.display());
        }
    }
}

fn read_image_attachment(
    path: &std::path::Path,
) -> Result<(String, String, Vec<u8>, AgentImagePreview)> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect dropped image {}", path.display()))?;
    anyhow::ensure!(metadata.is_file(), "drop target is not a regular file");
    anyhow::ensure!(
        metadata.len() <= MAX_AGENT_IMAGE_BYTES as u64,
        "image exceeds the {MAX_AGENT_IMAGE_BYTES} byte attachment limit"
    );
    let bytes =
        std::fs::read(path).with_context(|| format!("read dropped image {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= MAX_AGENT_IMAGE_BYTES,
        "image grew beyond the {MAX_AGENT_IMAGE_BYTES} byte attachment limit while being read"
    );
    let mime_type = image_mime_type(&bytes)
        .context("only PNG, JPEG, WebP, and GIF images can be attached")?
        .to_string();
    let preview = decode_agent_image_preview(&bytes).context("create dropped image preview")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("dropped-image")
        .to_string();
    Ok((name, mime_type, bytes, preview))
}

fn decode_agent_image_preview(bytes: &[u8]) -> Result<AgentImagePreview> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detect image format for preview")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_AGENT_IMAGE_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_AGENT_IMAGE_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_AGENT_IMAGE_DECODE_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().context("decode image for preview")?;
    let preview = decoded
        .thumbnail(AGENT_IMAGE_PREVIEW_WIDTH, AGENT_IMAGE_PREVIEW_HEIGHT)
        .into_rgba8();
    Ok(AgentImagePreview {
        width: preview.width(),
        height: preview.height(),
        rgba8: preview.into_raw(),
    })
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn queue_spawn_command(
    backend_commands: Option<&BackendCommandLane>,
    argv: Vec<String>,
) -> Result<()> {
    let program = argv
        .first()
        .context("key-policy spawn argv is empty")?
        .clone();
    let mut command = Vec::with_capacity(argv.len() + 1);
    command.push("spawn".to_string());
    command.extend(argv);
    queue_backend_command(backend_commands, format!("spawn-{program}"), command)
}

fn queue_backend_command(
    backend_commands: Option<&BackendCommandLane>,
    description: impl Into<String>,
    argv: Vec<String>,
) -> Result<()> {
    backend_commands
        .context("key-policy command requires a connected backend")?
        .try_send(description, argv)
}

fn pointer_target_for_cursor(
    renderer: &Renderer,
    cursor_position: Option<(f64, f64)>,
    grab: Option<&PointerGrab>,
) -> Option<PanePointerTarget> {
    if let Some(grab) = grab {
        return cursor_position
            .and_then(|(x, y)| {
                renderer.pointer_target_for_pane_at_window_point(grab.target.pane(), x, y)
            })
            .or(Some(grab.target));
    }
    cursor_position.and_then(|(x, y)| renderer.pointer_target_for_window_point(x, y))
}

fn route_pointer_motion(
    renderer: &Renderer,
    cursor_position: Option<(f64, f64)>,
    grab: &mut Option<PointerGrab>,
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    routing: &mut PointerRoutingState,
) {
    let target = pointer_target_for_cursor(renderer, cursor_position, grab.as_ref());
    if let (Some(grab), Some(target)) = (grab.as_mut(), target) {
        grab.target = target;
    }
    match target {
        Some(PanePointerTarget::Plugin { pane, x, y }) => {
            set_plugin_pointer_target(runtime, &mut routing.plugin_target, pane);
            routing.pending_plugin_motion = Some((pane, x, y));
            send_managed_pointer_leave(input_sender, &mut routing.managed_target);
        }
        Some(PanePointerTarget::Managed { pane, x, y }) => {
            routing.pending_plugin_motion = None;
            send_plugin_pointer_leave(runtime, &mut routing.plugin_target);
            if let Some(sender) = input_sender
                && send_input(
                    sender,
                    ClientMessage::Input(InputEvent::PointerMotion {
                        position: PanePosition { pane, x, y },
                    }),
                )
            {
                routing.managed_target = Some(pane);
            }
        }
        None => {
            routing.pending_plugin_motion = None;
            send_plugin_pointer_leave(runtime, &mut routing.plugin_target);
            send_managed_pointer_leave(input_sender, &mut routing.managed_target);
        }
    }
}

fn send_pointer_button_to_target(
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    routing: &mut PointerRoutingState,
    target: PanePointerTarget,
    button: PointerButtonState,
) {
    match target {
        PanePointerTarget::Plugin { pane, x, y } => {
            routing.pending_plugin_motion = None;
            set_plugin_pointer_target(runtime, &mut routing.plugin_target, pane);
            send_managed_pointer_leave(input_sender, &mut routing.managed_target);
            if let Err(error) = send_presentation_event(
                runtime,
                PresentationEvent::PointerButton {
                    pane,
                    x,
                    y,
                    button: button.button,
                    pressed: button.pressed,
                    modifiers: button.modifiers,
                },
            ) {
                eprintln!("dropped plugin pointer button: {error:#}");
            }
        }
        PanePointerTarget::Managed { pane, x, y } => {
            routing.pending_plugin_motion = None;
            send_plugin_pointer_leave(runtime, &mut routing.plugin_target);
            if let Some(sender) = input_sender
                && send_input(
                    sender,
                    ClientMessage::Input(InputEvent::PointerButton {
                        position: PanePosition { pane, x, y },
                        button: button.button,
                        pressed: button.pressed,
                    }),
                )
            {
                routing.managed_target = Some(pane);
            }
        }
    }
}

fn cancel_pointer_grab(
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    routing: &mut PointerRoutingState,
    grab: &mut Option<PointerGrab>,
    modifiers: u32,
) {
    let Some(grab) = grab.take() else {
        return;
    };
    let mut input_sender = input_sender;
    for button in grab.buttons {
        send_pointer_button_to_target(
            runtime,
            input_sender.as_deref_mut(),
            routing,
            grab.target,
            PointerButtonState {
                button,
                pressed: false,
                modifiers,
            },
        );
    }
}

fn set_plugin_pointer_target(
    runtime: Option<&PresentationWasmRuntime>,
    target: &mut Option<u64>,
    pane: u64,
) {
    if *target == Some(pane) {
        return;
    }
    send_plugin_pointer_leave(runtime, target);
    *target = Some(pane);
}

fn send_plugin_pointer_leave(runtime: Option<&PresentationWasmRuntime>, target: &mut Option<u64>) {
    let Some(pane) = target.take() else {
        return;
    };
    if let Err(error) = send_presentation_event(runtime, PresentationEvent::PointerLeft { pane }) {
        eprintln!("dropped plugin pointer leave: {error:#}");
    }
}

fn wheel_v120(delta: f32) -> i32 {
    let value = -f64::from(delta) * 120.0;
    if !value.is_finite() {
        return 0;
    }
    value.to_i32().unwrap_or_else(|| {
        if value.is_sign_negative() {
            i32::MIN
        } else {
            i32::MAX
        }
    })
}

fn flush_presentation_pointer_motion(
    runtime: Option<&PresentationWasmRuntime>,
    pending: &mut Option<(u64, f32, f32)>,
) -> Result<()> {
    let Some((pane, x, y)) = *pending else {
        return Ok(());
    };
    send_presentation_event(runtime, PresentationEvent::PointerMoved { pane, x, y })?;
    pending.take();
    Ok(())
}

fn send_managed_pointer_leave(lane: Option<&mut InputLane>, target: &mut Option<u64>) {
    let Some(previous) = target.take() else {
        return;
    };
    let Some(lane) = lane else {
        return;
    };
    if !send_input(lane, ClientMessage::Input(InputEvent::PointerLeave)) {
        *target = Some(previous);
    }
}

const fn linux_button_code(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(0x110),
        MouseButton::Right => Some(0x111),
        MouseButton::Middle => Some(0x112),
        MouseButton::Back => Some(0x116),
        MouseButton::Forward => Some(0x115),
        MouseButton::Other(_) => None,
    }
}

fn modifier_bits(modifiers: ModifiersState) -> u32 {
    u32::from(modifiers.control_key())
        | (u32::from(modifiers.alt_key()) << 1)
        | (u32::from(modifiers.shift_key()) << 2)
        | (u32::from(modifiers.super_key()) << 3)
}

fn key_policy_modifier_bits(modifiers: ModifiersState) -> i32 {
    (i32::from(modifiers.control_key()) * brood_wasm::ABI_MOD_CTRL)
        | (i32::from(modifiers.alt_key()) * brood_wasm::ABI_MOD_ALT)
        | (i32::from(modifiers.shift_key()) * brood_wasm::ABI_MOD_SHIFT)
}

impl ImportedFrame {
    #[allow(
        clippy::too_many_arguments,
        reason = "frame import requires the renderer resources plus transport identity and payload"
    )]
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bind_group_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        window_id: u64,
        resource_id: u64,
        sequence: u64,
        frame: FrameResource,
    ) -> Result<Self> {
        let (texture, view_format, source) = match frame {
            FrameResource::Dmabuf(frame) => {
                if frame.color_space != brood_ipc::FrameColorSpace::Srgb {
                    return Err(anyhow!(
                        "unsupported DMA-BUF color space {:?}",
                        frame.color_space
                    ));
                }
                let (format, view_format) = dmabuf_texture_formats(frame.fourcc)?;
                let texture = import_single_plane(
                    device,
                    frame.fd,
                    SinglePlaneDmabuf {
                        width: frame.width,
                        height: frame.height,
                        format,
                        view_format: Some(view_format),
                        modifier: frame.modifier,
                        stride: u64::from(frame.stride),
                        offset: u64::from(frame.offset),
                    },
                )?;
                eprintln!(
                    "imported window DMA-BUF window_id={} app_id={} resource_id={} size={}x{} format={:?} color_space={:?} stride={} modifier={} y_inverted={}",
                    window_id,
                    frame.app_id,
                    resource_id,
                    frame.width,
                    frame.height,
                    format,
                    frame.color_space,
                    frame.stride,
                    frame.modifier,
                    frame.y_inverted
                );
                (texture, view_format, ImportedFrameSource::Dmabuf)
            }
            FrameResource::SharedMemory(frame) => {
                let metadata = frame.image.metadata();
                if metadata.color_space != brood_protocol::FrameColorSpace::Srgb {
                    return Err(anyhow!(
                        "unsupported shared-memory color space {:?}",
                        metadata.color_space
                    ));
                }
                let (format, view_format) = match metadata.pixel_format {
                    brood_protocol::FramePixelFormat::Rgba8 => (
                        wgpu::TextureFormat::Rgba8Unorm,
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    brood_protocol::FramePixelFormat::Bgra8 => (
                        wgpu::TextureFormat::Bgra8Unorm,
                        wgpu::TextureFormat::Bgra8UnormSrgb,
                    ),
                };
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("hosted shared-memory image"),
                    size: wgpu::Extent3d {
                        width: metadata.width,
                        height: metadata.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[view_format],
                });
                write_shared_image(&texture, queue, &frame.image, sequence)?;
                eprintln!(
                    "uploaded window shared-memory image window_id={} app_id={} resource_id={} size={}x{} format={:?} color_space={:?} stride={}",
                    window_id,
                    frame.app_id,
                    resource_id,
                    metadata.width,
                    metadata.height,
                    format,
                    metadata.color_space,
                    metadata.stride
                );
                (
                    texture,
                    view_format,
                    ImportedFrameSource::SharedMemory(frame.image),
                )
            }
            FrameResource::Network(frame) => {
                if frame.color_space != brood_protocol::FrameColorSpace::Srgb {
                    return Err(anyhow!(
                        "unsupported network frame color space {:?}",
                        frame.color_space
                    ));
                }
                let (format, view_format) = match frame.pixel_format {
                    brood_protocol::FramePixelFormat::Rgba8 => (
                        wgpu::TextureFormat::Rgba8Unorm,
                        wgpu::TextureFormat::Rgba8UnormSrgb,
                    ),
                    brood_protocol::FramePixelFormat::Bgra8 => (
                        wgpu::TextureFormat::Bgra8Unorm,
                        wgpu::TextureFormat::Bgra8UnormSrgb,
                    ),
                };
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("hosted network image"),
                    size: wgpu::Extent3d {
                        width: frame.width,
                        height: frame.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[view_format],
                });
                write_packed_image(
                    &texture,
                    queue,
                    frame.width,
                    frame.height,
                    frame.stride,
                    &frame.pixels,
                )?;
                eprintln!(
                    "uploaded network image window_id={window_id} resource_id={resource_id} \
                     sequence={sequence} size={}x{} format={format:?} color_space={:?} stride={} \
                     compressed_bytes={} decoded_bytes={}",
                    frame.width,
                    frame.height,
                    frame.color_space,
                    frame.stride,
                    frame.compressed_bytes,
                    frame.pixels.len()
                );
                (texture, view_format, ImportedFrameSource::Network)
            }
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("hosted frame sRGB view"),
            format: Some(view_format),
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hosted frame bind group"),
            layout: bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        Ok(Self {
            texture,
            bind_group,
            source,
        })
    }

    fn update(&self, queue: &wgpu::Queue, sequence: u64) -> Result<()> {
        if let ImportedFrameSource::SharedMemory(image) = &self.source {
            write_shared_image(&self.texture, queue, image, sequence)?;
        }
        Ok(())
    }
}

fn write_shared_image(
    texture: &wgpu::Texture,
    queue: &wgpu::Queue,
    image: &brood_ipc::SharedImageConsumer,
    sequence: u64,
) -> Result<()> {
    let metadata = image.metadata();
    let pixels = image
        .pixels(sequence)
        .context("read published shared-memory frame")?;
    write_packed_image(
        texture,
        queue,
        metadata.width,
        metadata.height,
        metadata.stride,
        pixels,
    )
}

fn write_packed_image(
    texture: &wgpu::Texture,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    stride: u32,
    pixels: &[u8],
) -> Result<()> {
    let expected = usize::try_from(
        u64::from(stride)
            .checked_mul(u64::from(height))
            .context("packed image length overflow")?,
    )
    .context("packed image length exceeds address space")?;
    anyhow::ensure!(
        pixels.len() == expected,
        "packed image length mismatch: expected {expected}, received {}",
        pixels.len()
    );
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

fn create_frame_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
) -> (wgpu::BindGroupLayout, wgpu::Sampler, wgpu::RenderPipeline) {
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("hosted DMA-BUF sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("hosted DMA-BUF layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("hosted DMA-BUF shader"),
        source: wgpu::ShaderSource::Wgsl(
            "struct VertexOutput {
                    @builtin(position) position: vec4<f32>,
                    @location(0) uv: vec2<f32>,
                };
                @vertex fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
                    var positions = array<vec2<f32>, 3>(
                        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
                    var uvs = array<vec2<f32>, 3>(
                        vec2<f32>(0.0, 0.0), vec2<f32>(2.0, 0.0), vec2<f32>(0.0, 2.0));
                    var output: VertexOutput;
                    output.position = vec4<f32>(positions[index], 0.0, 1.0);
                    output.uv = uvs[index];
                    return output;
                }
                @group(0) @binding(0) var image: texture_2d<f32>;
                @group(0) @binding(1) var image_sampler: sampler;

                fn linear_to_srgb_channel(linear: f32) -> f32 {
                    return select(
                        12.92 * linear,
                        1.055 * pow(max(linear, 0.0), 1.0 / 2.4) - 0.055,
                        linear > 0.0031308);
                }
                fn linear_to_srgb(linear: vec3<f32>) -> vec3<f32> {
                    return vec3<f32>(
                        linear_to_srgb_channel(linear.r),
                        linear_to_srgb_channel(linear.g),
                        linear_to_srgb_channel(linear.b));
                }
                @fragment fn fragment_linear_target(
                    input: VertexOutput
                ) -> @location(0) vec4<f32> {
                    return textureSample(image, image_sampler, input.uv);
                }
                @fragment fn fragment_encoded_target(
                    input: VertexOutput
                ) -> @location(0) vec4<f32> {
                    let linear = textureSample(image, image_sampler, input.uv);
                    return vec4<f32>(linear_to_srgb(linear.rgb), linear.a);
                }"
            .into(),
        ),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("hosted DMA-BUF pipeline layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("hosted DMA-BUF pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some(if target_format.is_srgb() {
                "fragment_linear_target"
            } else {
                "fragment_encoded_target"
            }),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    (bind_group_layout, sampler, pipeline)
}

fn create_placeholder_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("managed window placeholder shader"),
        source: wgpu::ShaderSource::Wgsl(
            "struct VertexOutput {
                @builtin(position) position: vec4<f32>,
            };
            @vertex fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
                var positions = array<vec2<f32>, 3>(
                    vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
                var output: VertexOutput;
                output.position = vec4<f32>(positions[index], 0.0, 1.0);
                return output;
            }
            @fragment fn fragment(
                @builtin(position) position: vec4<f32>
            ) -> @location(0) vec4<f32> {
                let stripe = f32((u32(position.x) / 12u + u32(position.y) / 12u) % 2u);
                let base = 0.12 + stripe * 0.025;
                return vec4<f32>(base, base + 0.01, base + 0.025, 1.0);
            }"
            .into(),
        ),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("managed window placeholder pipeline layout"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("managed window placeholder pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fragment"),
            targets: &[Some(wgpu::ColorTargetState {
                format: target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn dmabuf_texture_formats(fourcc: u32) -> Result<(wgpu::TextureFormat, wgpu::TextureFormat)> {
    const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
    const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
    const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
    const DRM_FORMAT_XBGR8888: u32 = 0x3432_4258;
    match fourcc {
        DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => Ok((
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        )),
        DRM_FORMAT_ABGR8888 | DRM_FORMAT_XBGR8888 => Ok((
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )),
        _ => Err(anyhow!("unsupported happy-path DMA-BUF fourcc {fourcc:#x}")),
    }
}

fn srgb_surface_format(
    supported: &[wgpu::SurfaceFormatCapabilities],
) -> Option<wgpu::TextureFormat> {
    let mut encoded_fallback = None;
    for capability in supported {
        if !capability
            .color_spaces
            .contains(wgpu::SurfaceColorSpaces::SRGB)
        {
            continue;
        }
        if capability.format.is_srgb() {
            return Some(capability.format);
        }
        if encoded_fallback.is_none() {
            encoded_fallback = Some(capability.format);
        }
    }
    encoded_fallback
}

impl Renderer {
    fn new(
        window: Arc<Window>,
        fast_path_enabled: bool,
        plugin_graphics: PluginGraphicsBridge,
        generation: u64,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        pollster::block_on(Self::new_async(
            window,
            fast_path_enabled,
            plugin_graphics,
            generation,
            proxy,
        ))
    }

    async fn new_async(
        window: Arc<Window>,
        fast_path_enabled: bool,
        plugin_graphics: PluginGraphicsBridge,
        generation: u64,
        proxy: winit::event_loop::EventLoopProxy<ClientEvent>,
    ) -> Result<Self> {
        let instance = if fast_path_enabled {
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = wgpu::Backends::VULKAN;
            wgpu::Instance::new(descriptor)
        } else {
            wgpu::Instance::default()
        };
        let surface = instance
            .create_surface(Arc::clone(&window))
            .context("create wgpu surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .context("find a wgpu adapter")?;
        let required_features = if fast_path_enabled {
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("brood-client device"),
                required_features,
                ..Default::default()
            })
            .await
            .context("create wgpu device")?;
        let lost_proxy = proxy.clone();
        device.set_device_lost_callback(move |reason, message| {
            let _send_result = lost_proxy.send_event(ClientEvent::RendererFailed {
                generation,
                message: format!("wgpu device lost ({reason:?}): {message}"),
            });
        });
        device.on_uncaptured_error(Arc::new(move |error| {
            let _send_result = proxy.send_event(ClientEvent::RendererFailed {
                generation,
                message: format!("uncaptured wgpu error: {error}"),
            });
        }));

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow!("wgpu surface has no supported configuration"))?;
        let capabilities = surface.get_capabilities(&adapter);
        config.format = srgb_surface_format(&capabilities.format_capabilities)
            .ok_or_else(|| anyhow!("wgpu surface cannot present standard sRGB"))?;
        config.color_space = wgpu::SurfaceColorSpace::Srgb;
        config.present_mode = low_latency_present_mode(&capabilities.present_modes);
        config.desired_maximum_frame_latency = 1;
        let (frame_bind_group_layout, frame_sampler, frame_pipeline) =
            create_frame_pipeline(&device, config.format);
        let placeholder_pipeline = create_placeholder_pipeline(&device, config.format);
        let plugin_canvas =
            GpuCanvasRenderer::new(&device, &queue, &frame_bind_group_layout, &frame_sampler);
        surface.configure(&device, &config);
        eprintln!(
            "configured presentation surface format={:?} color_space={:?} \
             present_mode={:?} maximum_frame_latency={}",
            config.format,
            config.color_space,
            config.present_mode,
            config.desired_maximum_frame_latency,
        );
        Ok(Self {
            generation,
            window,
            surface,
            device,
            queue,
            config,
            frame_bind_group_layout,
            frame_sampler,
            frame_pipeline,
            placeholder_pipeline,
            plugin_canvas,
            imported_frames: HashMap::new(),
            frame_keys: BTreeMap::new(),
            next_frame_key: 1,
            available_windows: Vec::new(),
            active_session: 0,
            active_backend: None,
            active_sessions: BTreeMap::new(),
            current_windows: Vec::new(),
            current_sequence: None,
            current_sequences: BTreeMap::new(),
            plugin_graphics,
            plugin_panes: BTreeMap::new(),
            plugin_pane_realms: BTreeMap::new(),
            focused_pane: None,
            pending_focus_pane: None,
            accepted_layout: ResolvedLayout::empty(),
            layout_source: LayoutSource::NativeFallback,
            layout_state_dirty: true,
            sent_managed_sizes: HashMap::new(),
            sent_plugin_sizes: HashMap::new(),
            window_resize_in_progress: false,
        })
    }

    fn logical_extent(&self) -> (f32, f32, f32) {
        let scale = self.window.scale_factor() as f32;
        (
            self.config.width as f32 / scale,
            self.config.height as f32 / scale,
            scale,
        )
    }

    fn select_backend_snapshot(
        &mut self,
        backend: BackendId,
        active_session: u64,
        windows: Vec<PresentationWindowSummary>,
    ) -> Option<FocusChange> {
        let previous_inventory = self
            .available_windows
            .iter()
            .filter(|window| window.backend == backend)
            .map(|window| {
                (
                    window.backend,
                    window.pane,
                    window.session,
                    window.app_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let previous_active = self.active_sessions.clone();
        let mut available_windows = windows
            .into_iter()
            .map(|window| AvailableWindow {
                backend,
                pane: window.pane,
                session: window.session,
                app_id: window.app_id,
            })
            .collect::<Vec<_>>();
        available_windows.sort_by_key(|window| window.pane);
        let inventory = available_windows
            .iter()
            .map(|window| {
                (
                    window.backend,
                    window.pane,
                    window.session,
                    window.app_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut merged_windows = self
            .available_windows
            .iter()
            .filter(|window| window.backend != backend)
            .cloned()
            .collect::<Vec<_>>();
        merged_windows.extend(available_windows);
        merged_windows.sort_by_key(|window| (window.backend, window.pane));
        if previous_inventory == inventory
            && previous_active.get(&backend).copied() == Some(active_session)
        {
            return None;
        }

        self.available_windows = merged_windows;
        self.active_sessions.insert(backend, active_session);
        if self.active_backend.is_none() || self.active_backend == Some(backend) {
            self.active_backend = Some(backend);
            self.active_session = active_session;
        }
        self.layout_state_dirty = true;
        if self.layout_source == LayoutSource::NativeFallback {
            return self.apply_default_session_layout(self.active_session);
        }
        self.window.request_redraw();
        self.reconcile_layout()
    }

    fn disconnect_backend(&mut self, backend: BackendId) -> Option<FocusChange> {
        let previous_focus = self.focused_pane;
        let removed_panes = self
            .available_windows
            .iter()
            .filter(|window| window.backend == backend)
            .map(|window| window.pane)
            .collect::<HashSet<_>>();
        self.available_windows
            .retain(|window| window.backend != backend);
        self.current_windows
            .retain(|window| window.backend != backend);
        self.current_sequences.remove(&backend);
        self.active_sessions.remove(&backend);
        let removed_resources = self
            .frame_keys
            .iter()
            .filter(|((owner, _resource), _key)| *owner == backend)
            .map(|(_qualified, key)| *key)
            .collect::<Vec<_>>();
        self.frame_keys
            .retain(|(owner, _resource), _key| *owner != backend);
        for resource in removed_resources {
            self.imported_frames.remove(&resource);
        }
        if self
            .focused_pane_id()
            .is_some_and(|pane| removed_panes.contains(&pane))
        {
            self.focused_pane = None;
        }
        if self.active_backend == Some(backend) {
            self.active_backend = self.active_sessions.keys().next().copied();
            self.active_session = self
                .active_backend
                .and_then(|active| self.active_sessions.get(&active).copied())
                .unwrap_or_default();
        }
        self.layout_state_dirty = true;
        self.window.request_redraw();
        self.reconcile_layout()
            .or_else(|| focus_change_between(previous_focus, self.focused_pane))
    }

    fn select_active_session(
        &mut self,
        backend: BackendId,
        active_session: u64,
    ) -> Option<FocusChange> {
        let backend_was_already_active =
            self.active_sessions.get(&backend).copied() == Some(active_session);
        if backend_was_already_active
            && self.active_backend == Some(backend)
            && self.active_session == active_session
        {
            return None;
        }
        self.active_backend = Some(backend);
        self.active_sessions.insert(backend, active_session);
        if self.layout_source == LayoutSource::Actor {
            self.active_session = active_session;
            self.layout_state_dirty = true;
            self.window.request_redraw();
            return self.reconcile_layout();
        }
        self.apply_default_session_layout(active_session)
    }

    fn apply_default_session_layout(&mut self, active_session: u64) -> Option<FocusChange> {
        self.active_session = active_session;
        self.layout_state_dirty = true;
        let visible = self.default_visible_pane_inventory();
        let previous_focus = self.focused_pane;
        let focus_change = self.commit_layout(
            native_flat_layout(&visible, self.layout_extent()),
            self.focused_pane_id().filter(|pane| visible.contains(pane)),
            LayoutSource::NativeFallback,
        );
        self.window.request_redraw();
        focus_change.or_else(|| focus_change_between(previous_focus, self.focused_pane))
    }

    fn select_scene(
        &mut self,
        backend: BackendId,
        sequence: u64,
        _width: u32,
        _height: u32,
        windows: Vec<SceneWindowUpdate>,
    ) -> Result<(Option<FocusChange>, Vec<u64>)> {
        let previous_focus = self.focused_pane;
        let live_resources = windows
            .iter()
            .map(|window| window.resource_id)
            .collect::<HashSet<_>>();
        let previous_managed_panes = self
            .current_windows
            .iter()
            .map(|window| window.window_id)
            .collect::<Vec<_>>();
        let mut scene_windows = Vec::with_capacity(windows.len());
        let mut backend_focus = None;
        for window in windows {
            anyhow::ensure!(
                window.backend == backend,
                "scene window backend identity does not match its scene"
            );
            let previous_rect = self
                .current_windows
                .iter()
                .find(|current| {
                    current.backend == window.backend && current.window_id == window.window_id
                })
                .map_or(window.rect, |current| current.rect);
            let resource_key = self.resource_key(backend, window.resource_id);
            if let Some(frame) = window.resource {
                let imported = ImportedFrame::new(
                    &self.device,
                    &self.queue,
                    &self.frame_bind_group_layout,
                    &self.frame_sampler,
                    window.window_id,
                    resource_key,
                    sequence,
                    frame,
                )?;
                self.imported_frames.insert(resource_key, imported);
            }
            if let Some(imported) = self.imported_frames.get(&resource_key) {
                imported.update(&self.queue, sequence)?;
            }
            if window.active {
                backend_focus = Some(window.window_id);
            }
            scene_windows.push(SceneWindow {
                backend,
                window_id: window.window_id,
                resource_id: resource_key,
                source_rect: window.rect,
                rect: previous_rect,
                active: window.active,
            });
        }
        scene_windows.sort_by_key(|window| window.window_id);
        if scene_windows
            .iter()
            .all(|window| self.imported_frames.contains_key(&window.resource_id))
        {
            self.current_windows
                .retain(|window| window.backend != backend);
            self.current_windows.extend(scene_windows);
            self.current_windows
                .sort_by_key(|window| (window.backend, window.window_id));
            self.current_sequence = Some(sequence);
            self.current_sequences.insert(backend, sequence);
            let managed_panes = self
                .current_windows
                .iter()
                .map(|window| window.window_id)
                .collect::<Vec<_>>();
            let newly_active_pane = backend_focus
                .filter(|pane| !previous_managed_panes.contains(pane))
                .filter(|pane| managed_panes.contains(pane));
            self.focused_pane = scene_focus_target(
                if newly_active_pane.is_some() {
                    None
                } else {
                    previous_focus
                },
                self.visible_plugin_panes(),
                &managed_panes,
                backend_focus,
            );
            self.layout_state_dirty |= previous_focus != self.focused_pane;
            let _focus_change = self.reconcile_layout();
            let focus_change =
                focus_change_between(previous_focus, self.focused_pane).or_else(|| {
                    let Some(FocusedPane::Managed(local_focus)) = self.focused_pane else {
                        return None;
                    };
                    (backend_focus != Some(local_focus)).then_some(FocusChange {
                        plugin_blur: None,
                        plugin_focus: None,
                        managed_window: Some(local_focus),
                        reset_keyboard: false,
                    })
                });
            self.window.request_redraw();
            let stale_resources = self
                .frame_keys
                .iter()
                .filter(|((owner, resource), _key)| {
                    *owner == backend && !live_resources.contains(resource)
                })
                .map(|((owner, resource), key)| ((*owner, *resource), *key))
                .collect::<Vec<_>>();
            for (qualified, key) in &stale_resources {
                self.frame_keys.remove(qualified);
                self.imported_frames.remove(key);
            }
            return Ok((
                focus_change,
                stale_resources
                    .into_iter()
                    .map(|((_owner, resource), _key)| resource)
                    .collect(),
            ));
        }
        Ok((None, Vec::new()))
    }

    fn resource_key(&mut self, backend: BackendId, resource: u64) -> u64 {
        if let Some(key) = self.frame_keys.get(&(backend, resource)) {
            return *key;
        }
        let key = self.next_frame_key.max(1);
        self.next_frame_key = key.saturating_add(1);
        self.frame_keys.insert((backend, resource), key);
        key
    }

    fn frame_sequences(&self) -> Vec<(BackendId, u64)> {
        self.current_sequences
            .iter()
            .map(|(backend, sequence)| (*backend, *sequence))
            .collect()
    }

    const fn focused_plugin_pane(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Plugin(pane)) => Some(pane),
            _ => None,
        }
    }

    const fn focused_managed_window(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Managed(window_id)) => Some(window_id),
            _ => None,
        }
    }

    fn focus_pane(&mut self, pane: u64) -> Option<FocusChange> {
        let Some(target) = focus_request_target(
            &self.accepted_layout,
            pane,
            self.focus_target_for_pane(pane),
        ) else {
            self.pending_focus_pane = Some(pane);
            return None;
        };
        self.pending_focus_pane = None;
        self.set_focus(target)
    }

    fn pane_for_app_id(&self, app_id: &str) -> Option<u64> {
        self.available_windows
            .iter()
            .find_map(|window| (window.app_id == app_id).then_some(window.pane))
    }

    fn key_policy_snapshot(&self) -> KeyPolicySnapshot {
        KeyPolicySnapshot {
            focused_pane: self.focused_pane_id(),
            panes: self.default_visible_pane_inventory(),
            managed_panes: self
                .available_windows
                .iter()
                .filter(|window| self.selected_session_is(window.backend, window.session))
                .map(|window| window.pane)
                .collect(),
            app_panes: self
                .available_windows
                .iter()
                .filter(|window| {
                    self.selected_session_is(window.backend, window.session)
                        && !window.app_id.is_empty()
                })
                .map(|window| KeyPolicyAppPane {
                    pane: window.pane,
                    app_id: window.app_id.clone(),
                })
                .collect(),
        }
    }

    fn pane_at_window_point(&self, x: f64, y: f64) -> Option<u64> {
        let (x, y) = self.scene_position_for_window_point(x, y);
        let (width, height) = self.composition_extent();
        let bounds = LayoutRect {
            x: 0,
            y: 0,
            w: i32::try_from(width).unwrap_or(i32::MAX).max(1),
            h: i32::try_from(height).unwrap_or(i32::MAX).max(1),
        };
        let hit = self
            .accepted_layout
            .hit_stack(LocalPoint::from_pixel_in_rect(x, y, bounds))
            .into_iter()
            .next()?;
        let LayoutItemKind::Pane { pane } = hit.kind else {
            return None;
        };
        Some(pane)
    }

    fn set_focus(&mut self, target: FocusedPane) -> Option<FocusChange> {
        let previous = self.focused_pane;
        if previous == Some(target) {
            return None;
        }
        self.focused_pane = Some(target);
        if let FocusedPane::Managed(pane) = target
            && let Some(window) = self
                .available_windows
                .iter()
                .find(|window| window.pane == pane)
        {
            self.active_backend = Some(window.backend);
            self.active_session = window.session;
        }
        self.layout_state_dirty = true;
        let target = self.focused_pane?;
        if previous == Some(target) {
            return None;
        }
        for window in &mut self.current_windows {
            window.active = matches!(target, FocusedPane::Managed(id) if id == window.window_id);
        }
        self.window.request_redraw();
        focus_change_between(previous, Some(target))
    }

    fn scene_position_for_window_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (content_width, content_height) = self.composition_extent();
        let window_size = self.window.inner_size();
        (
            x * f64::from(content_width) / f64::from(window_size.width.max(1)),
            y * f64::from(content_height) / f64::from(window_size.height.max(1)),
        )
    }

    fn pointer_target_for_window_point(&self, x: f64, y: f64) -> Option<PanePointerTarget> {
        let pane = self.pane_at_window_point(x, y)?;
        self.pointer_target_for_pane_at_window_point(pane, x, y)
    }

    fn pointer_target_for_pane_at_window_point(
        &self,
        pane: u64,
        x: f64,
        y: f64,
    ) -> Option<PanePointerTarget> {
        let (x, y) = self.scene_position_for_window_point(x, y);
        if let Some(plugin) = self.plugin_panes.get(&pane) {
            let scale = self.window.scale_factor() as f32;
            let (x, y) = plugin_local_point(x, y, plugin.rect, scale);
            return Some(PanePointerTarget::Plugin { pane, x, y });
        }
        let window = self
            .current_windows
            .iter()
            .find(|window| window.window_id == pane)?;
        let (x, y) = project_point_between_rects(x, y, window.rect, window.source_rect);
        Some(PanePointerTarget::Managed { pane, x, y })
    }

    fn composition_extent(&self) -> (u32, u32) {
        (self.config.width.max(1), self.config.height.max(1))
    }

    fn reconcile_layout(&mut self) -> Option<FocusChange> {
        let panes = self.pane_inventory();
        let pending_focus = self.pending_focus_pane.filter(|pane| panes.contains(pane));
        let valid = self
            .accepted_layout
            .clone()
            .validate_for_panes(&panes)
            .is_ok();
        if self.layout_source == LayoutSource::NativeFallback {
            let extent = self.layout_extent();
            let focus = self.focused_pane_id();
            let change = self.commit_layout(
                native_flat_layout(&self.default_visible_pane_inventory(), extent),
                pending_focus.or(focus),
                LayoutSource::NativeFallback,
            );
            self.clear_fulfilled_pending_focus(pending_focus);
            return change;
        }
        if !valid {
            eprintln!(
                "retaining accepted actor layout until its replacement is ready inventory_panes={}",
                panes.len()
            );
        }
        let change = self.project_accepted_layout(pending_focus);
        self.clear_fulfilled_pending_focus(pending_focus);
        change
    }

    fn clear_fulfilled_pending_focus(&mut self, requested: Option<u64>) {
        if let Some(requested) = requested
            && Some(requested) == self.focused_pane_id()
        {
            self.pending_focus_pane = None;
            eprintln!("fulfilled deferred pane focus pane={requested}");
        }
    }

    fn commit_actor_layout(
        &mut self,
        layout: ResolvedLayout,
        focus: Option<u64>,
    ) -> (Option<FocusChange>, bool) {
        let panes = self.pane_inventory();
        match layout.validate_for_panes(&panes) {
            Ok(layout) => {
                let pending_focus = self
                    .pending_focus_pane
                    .filter(|pane| layout.pane_item(*pane).is_some());
                let change =
                    self.commit_layout(layout, pending_focus.or(focus), LayoutSource::Actor);
                self.clear_fulfilled_pending_focus(pending_focus);
                (change, true)
            }
            Err(error) => self.reject_actor_layout(&error),
        }
    }

    fn reject_actor_layout(
        &mut self,
        error: &dyn std::fmt::Display,
    ) -> (Option<FocusChange>, bool) {
        if self.layout_source == LayoutSource::Actor {
            eprintln!("rejected actor-submitted layout; retaining last accepted layout: {error}");
            return (self.project_accepted_layout(None), false);
        }
        eprintln!("rejected actor-submitted layout; using native fallback: {error}");
        let visible = self.default_visible_pane_inventory();
        let extent = self.layout_extent();
        let focus = self.focused_pane_id();
        (
            self.commit_layout(
                native_flat_layout(&visible, extent),
                focus,
                LayoutSource::NativeFallback,
            ),
            false,
        )
    }

    fn commit_layout(
        &mut self,
        layout: ResolvedLayout,
        focus: Option<u64>,
        source: LayoutSource,
    ) -> Option<FocusChange> {
        if self.layout_source != source {
            let source = match source {
                LayoutSource::NativeFallback => "native-fallback",
                LayoutSource::Actor => "actor",
            };
            eprintln!("presentation layout source changed source={source}");
        }
        self.layout_source = source;
        self.accepted_layout = layout;
        self.project_accepted_layout(focus)
    }

    fn project_accepted_layout(&mut self, requested_focus: Option<u64>) -> Option<FocusChange> {
        let previous_focus = self.focused_pane;
        let extent = self.layout_extent();

        for plugin in self.plugin_panes.values_mut() {
            plugin.rect = [0, 0, 0, 0];
        }
        for window in &mut self.current_windows {
            window.rect = [0, 0, 0, 0];
        }
        for item in &self.accepted_layout.items {
            let LayoutItemKind::Pane { pane } = item.kind else {
                continue;
            };
            let rect = item.pixel_rect(extent);
            let rect = [rect.x, rect.y, rect.w, rect.h];
            if let Some(plugin) = self.plugin_panes.get_mut(&pane) {
                plugin.rect = rect;
            } else if let Some(window) = self
                .current_windows
                .iter_mut()
                .find(|window| window.window_id == pane)
            {
                window.rect = rect;
            }
        }

        let requested_focus =
            requested_focus.and_then(|pane| self.focus_target_for_visible_pane(pane));
        let retained_focus = self
            .focused_pane_id()
            .and_then(|pane| self.focus_target_for_visible_pane(pane));
        let fallback_focus = self
            .accepted_layout
            .items
            .iter()
            .find_map(|item| match item.kind {
                LayoutItemKind::Pane { pane } => Some(pane),
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => None,
            })
            .and_then(|pane| self.focus_target_for_pane(pane));
        self.focused_pane = requested_focus.or(retained_focus).or(fallback_focus);
        for window in &mut self.current_windows {
            window.active = self.focused_pane == Some(FocusedPane::Managed(window.window_id));
        }
        self.sent_managed_sizes.retain(|window, _| {
            self.current_windows
                .iter()
                .any(|item| item.window_id == *window)
        });
        self.sent_plugin_sizes
            .retain(|pane, _| self.plugin_pane_realms.contains_key(pane));
        focus_change_between(previous_focus, self.focused_pane)
    }

    fn layout_extent(&self) -> LayoutSize {
        let (width, height) = self.composition_extent();
        LayoutSize {
            w: i32::try_from(width).unwrap_or(i32::MAX).max(1),
            h: i32::try_from(height).unwrap_or(i32::MAX).max(1),
        }
    }

    fn layout_state_message(
        &self,
        revision: u64,
        lease: brood_layout::LayoutLeaseToken,
        owned_panes: &[u64],
        focused_pane: Option<u64>,
        size: LayoutSize,
        rest: bool,
    ) -> LayoutStateMessage {
        let default_visible = self.default_visible_pane_inventory();
        let visible_panes = owned_panes
            .iter()
            .copied()
            .filter(|pane| default_visible.contains(pane))
            .collect::<Vec<_>>();
        let visible = visible_panes
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        LayoutStateMessage {
            message_type: "layout-state",
            revision,
            lease: brood_wasm::LayoutLeaseToken {
                id: lease.id.0,
                generation: lease.generation,
            },
            visible_panes,
            active_session: self.active_session,
            available_windows: self
                .available_windows
                .iter()
                .filter(|window| owned_panes.contains(&window.pane))
                .cloned()
                .collect(),
            pane_realms: layout_pane_realms(
                &self.available_windows,
                &self.plugin_pane_realms,
                owned_panes,
            ),
            focused_pane,
            pane_app_ids: self
                .available_windows
                .iter()
                .filter(|window| !window.app_id.is_empty() && visible.contains(&window.pane))
                .map(|window| KeyPolicyAppPane {
                    pane: window.pane,
                    app_id: window.app_id.clone(),
                })
                .collect(),
            size: LayoutStateSize {
                w: size.w,
                h: size.h,
            },
            rest,
        }
    }

    fn pane_inventory(&self) -> Vec<u64> {
        pane_inventory_from_catalogs(&self.plugin_pane_realms, &self.available_windows)
    }

    fn default_visible_pane_inventory(&self) -> Vec<u64> {
        self.visible_plugin_panes()
            .into_iter()
            .chain(
                self.available_windows
                    .iter()
                    .filter(|window| self.selected_session_is(window.backend, window.session))
                    .map(|window| window.pane),
            )
            .collect()
    }

    fn selected_session_is(&self, backend: BackendId, session: u64) -> bool {
        self.active_backend == Some(backend) && self.active_session == session
    }

    fn visible_plugin_panes(&self) -> Vec<u64> {
        visible_plugin_panes_for_realm(
            &self.plugin_pane_realms,
            self.active_backend,
            self.active_session,
        )
    }

    fn set_plugin_pane_realms(&mut self, sessions: &[(u64, Option<(BackendId, u64)>)]) {
        let updated = sessions.iter().copied().collect::<BTreeMap<_, _>>();
        if self.plugin_pane_realms != updated {
            self.plugin_pane_realms = updated;
            self.layout_state_dirty = true;
            self.window.request_redraw();
        }
    }

    const fn focused_pane_id(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Plugin(pane) | FocusedPane::Managed(pane)) => Some(pane),
            None => None,
        }
    }

    fn focus_target_for_pane(&self, pane: u64) -> Option<FocusedPane> {
        if self.visible_plugin_panes().contains(&pane) {
            return Some(FocusedPane::Plugin(pane));
        }
        self.available_windows
            .iter()
            .any(|window| window.pane == pane)
            .then_some(FocusedPane::Managed(pane))
    }

    fn focus_target_for_visible_pane(&self, pane: u64) -> Option<FocusedPane> {
        self.accepted_layout
            .pane_item(pane)
            .and_then(|_| self.focus_target_for_pane(pane))
    }

    fn refresh_plugin_graphics(
        &mut self,
        pane: u64,
        expected_sequence: u64,
    ) -> Result<Option<FocusChange>> {
        let Some(snapshot) = self.plugin_graphics.snapshot(pane)? else {
            return Ok(None);
        };
        if snapshot.sequence < expected_sequence {
            return Ok(None);
        }
        let target = self.plugin_scene_extent(pane);
        let target_changed = self.plugin_panes.get(&pane).is_none_or(|current| {
            current.scene.width != target.0
                || current.scene.height != target.1
                || current.scene.scale.to_bits() != target.2.to_bits()
        });
        if self
            .plugin_panes
            .get(&pane)
            .is_some_and(|current| current.scene.sequence >= snapshot.sequence && !target_changed)
        {
            return Ok(None);
        }
        let scene = self.plugin_canvas.compile(
            &self.device,
            &self.queue,
            &snapshot,
            target,
            self.plugin_panes.get(&pane).map(|current| &current.scene),
        )?;
        if target_changed
            || snapshot.sequence <= 3
            || snapshot.sequence.is_multiple_of(120)
            || std::env::var_os("BROOD_TRACE_PLUGIN_GRAPHICS").is_some()
        {
            eprintln!(
                "accepted plugin draw packet pane={} sequence={} commands={} gpu_target={}x{} scale={:.2}",
                snapshot.pane,
                snapshot.sequence,
                snapshot.packet.commands.len(),
                target.0,
                target.1,
                target.2,
            );
        }
        let new_plugin_pane = !self.plugin_panes.contains_key(&pane);
        let rect = self.plugin_panes.get(&pane).map_or_else(
            || {
                [
                    0,
                    0,
                    i32::try_from(self.config.width).unwrap_or(i32::MAX),
                    i32::try_from(self.config.height).unwrap_or(i32::MAX),
                ]
            },
            |current| current.rect,
        );
        self.plugin_panes
            .insert(pane, PluginPaneTexture { scene, rect });
        self.layout_state_dirty |= new_plugin_pane;
        let focus_change = self.reconcile_layout();
        let updated_target = self.plugin_scene_extent(pane);
        if updated_target.0 != target.0
            || updated_target.1 != target.1
            || updated_target.2.to_bits() != target.2.to_bits()
        {
            let scene = self.plugin_canvas.compile(
                &self.device,
                &self.queue,
                &snapshot,
                updated_target,
                self.plugin_panes.get(&pane).map(|current| &current.scene),
            )?;
            if let Some(plugin) = self.plugin_panes.get_mut(&pane) {
                plugin.scene = scene;
            }
        }
        self.window.request_redraw();
        Ok(focus_change)
    }

    fn sync_plugin_graphics_extents(&mut self) -> Result<()> {
        if self.window_resize_in_progress {
            return Ok(());
        }
        let scenes = self
            .plugin_panes
            .values()
            .map(|plugin| (plugin.scene.pane, plugin.scene.sequence))
            .collect::<Vec<_>>();
        for (pane, sequence) in scenes {
            let _focus_change = self.refresh_plugin_graphics(pane, sequence)?;
        }
        Ok(())
    }

    fn remove_plugin_pane(&mut self, pane: u64) -> Option<FocusChange> {
        let removed = self.plugin_panes.remove(&pane).is_some();
        self.plugin_pane_realms.remove(&pane);
        self.sent_plugin_sizes.remove(&pane);
        if !removed {
            return None;
        }
        self.layout_state_dirty = true;
        let mut focus_change = self.reconcile_layout();
        if let Some(change) = &mut focus_change
            && change.plugin_blur == Some(pane)
        {
            change.plugin_blur = None;
        }
        self.window.request_redraw();
        focus_change
    }

    fn plugin_scene_extent(&self, pane: u64) -> (u32, u32, f32) {
        let scale = self.window.scale_factor() as f32;
        if self.window_resize_in_progress
            && let Some(plugin) = self.plugin_panes.get(&pane)
        {
            return (plugin.scene.width, plugin.scene.height, plugin.scene.scale);
        }
        let extent = self.plugin_panes.get(&pane).map_or_else(
            || (self.config.width.max(1), self.config.height.max(1)),
            |plugin| {
                positive_rect_extent(plugin.rect)
                    .unwrap_or((plugin.scene.width, plugin.scene.height))
            },
        );
        (extent.0, extent.1, scale)
    }

    fn pending_plugin_resize_events(&self) -> Vec<(u64, PluginPaneSize)> {
        if self.window_resize_in_progress {
            return Vec::new();
        }
        let scale = self.window.scale_factor() as f32;
        let extent = self.layout_extent();
        self.plugin_pane_realms
            .keys()
            .filter_map(|pane| {
                let item = self.accepted_layout.pane_item(*pane)?;
                let rect = item.pixel_rect(extent);
                let (width, height) = positive_rect_extent([rect.x, rect.y, rect.w, rect.h])?;
                let size = PluginPaneSize {
                    width: width as f32 / scale,
                    height: height as f32 / scale,
                    scale,
                };
                (self.sent_plugin_sizes.get(pane) != Some(&size)).then_some((*pane, size))
            })
            .collect()
    }

    fn mark_plugin_resize_sent(&mut self, pane: u64, size: PluginPaneSize) {
        self.sent_plugin_sizes.insert(pane, size);
    }

    fn resize_window_live(&mut self, width: u32, height: u32) -> Option<FocusChange> {
        self.window_resize_in_progress = true;
        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
        let focus_change = self.project_accepted_layout(None);
        self.window.request_redraw();
        focus_change
    }

    fn finish_window_resize(&mut self) -> Result<Option<FocusChange>> {
        self.window_resize_in_progress = false;
        self.layout_state_dirty = true;
        let focus_change = self.reconcile_layout();
        self.sync_plugin_graphics_extents()?;
        self.window.request_redraw();
        Ok(focus_change)
    }

    fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.config);
        self.window.request_redraw();
    }

    fn render(&mut self) -> RenderOutcome {
        let started = Instant::now();
        let (frame, reconfigure) = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) => (frame, false),
            CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                return RenderOutcome::Skipped;
            }
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                return RenderOutcome::Reconfigure;
            }
            CurrentSurfaceTexture::Validation => {
                return RenderOutcome::Failed("wgpu surface validation failed");
            }
        };
        let acquired = Instant::now();
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("brood-client frame encoder"),
            });

        for plugin in self.plugin_panes.values_mut() {
            if let Err(error) = self
                .plugin_canvas
                .encode_if_dirty(&mut encoder, &mut plugin.scene)
            {
                eprintln!("failed to encode plugin GPU scene: {error:#}");
                return RenderOutcome::Failed("failed to encode plugin GPU scene");
            }
        }

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("brood-client clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(Color {
                            r: 0.04,
                            g: 0.05,
                            b: 0.07,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let extent = self.composition_extent();
            for pane in self.layout_panes_in_render_order() {
                if let Some(plugin) = self.plugin_panes.get(&pane) {
                    let Some(rect) = clip_rect_to_extent(plugin.rect, extent) else {
                        continue;
                    };
                    let viewport =
                        scaled_viewport(rect, extent, (self.config.width, self.config.height));
                    render_pass.set_viewport(
                        viewport[0],
                        viewport[1],
                        viewport[2],
                        viewport[3],
                        0.0,
                        1.0,
                    );
                    render_pass.set_pipeline(&self.frame_pipeline);
                    render_pass.set_bind_group(0, plugin.scene.output_bind_group(), &[]);
                    render_pass.draw(0..3, 0..1);
                    continue;
                }

                let window = self
                    .current_windows
                    .iter()
                    .find(|window| window.window_id == pane);
                let imported =
                    window.and_then(|window| self.imported_frames.get(&window.resource_id));
                let rect = window
                    .map(|window| window.rect)
                    .or_else(|| {
                        self.accepted_layout.pane_item(pane).map(|item| {
                            let rect = item.pixel_rect(self.layout_extent());
                            [rect.x, rect.y, rect.w, rect.h]
                        })
                    })
                    .and_then(|rect| clip_rect_to_extent(rect, extent));
                let Some(rect) = rect else {
                    continue;
                };
                let available = self
                    .available_windows
                    .iter()
                    .any(|window| window.pane == pane);
                let frame_ready = imported.is_some();
                let viewport =
                    scaled_viewport(rect, extent, (self.config.width, self.config.height));
                render_pass.set_viewport(
                    viewport[0],
                    viewport[1],
                    viewport[2],
                    viewport[3],
                    0.0,
                    1.0,
                );
                if let Some(imported) = imported {
                    render_pass.set_pipeline(&self.frame_pipeline);
                    render_pass.set_bind_group(0, &imported.bind_group, &[]);
                } else if managed_pane_uses_placeholder(available, frame_ready) {
                    render_pass.set_pipeline(&self.placeholder_pipeline);
                } else {
                    continue;
                }
                render_pass.draw(0..3, 0..1);
            }
            drop(render_pass);
        }

        let encoded = Instant::now();
        let submission = self.queue.submit([encoder.finish()]);
        let submitted = Instant::now();
        if self.current_sequence.is_some()
            && self
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(std::time::Duration::from_secs(1)),
                })
                .is_err()
        {
            return RenderOutcome::Failed("timed out waiting for composed frame consumption");
        }
        self.window.pre_present_notify();
        self.queue.present(frame);
        let presented = Instant::now();
        RenderOutcome::Presented {
            sequence: self.current_sequence,
            reconfigure,
            timings: RenderTimings {
                acquire_us: acquired.duration_since(started).as_micros(),
                encode_us: encoded.duration_since(acquired).as_micros(),
                submit_us: submitted.duration_since(encoded).as_micros(),
                present_us: presented.duration_since(submitted).as_micros(),
            },
        }
    }

    fn layout_panes_in_render_order(&self) -> Vec<u64> {
        let mut panes = self
            .accepted_layout
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item.kind {
                LayoutItemKind::Pane { pane } => Some((item.z, index, pane)),
                LayoutItemKind::Placeholder
                | LayoutItemKind::Handle
                | LayoutItemKind::Chrome
                | LayoutItemKind::Overlay => None,
            })
            .collect::<Vec<_>>();
        panes.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        panes.into_iter().map(|(_, _, pane)| pane).collect()
    }

    fn pending_managed_resize_requests(&self) -> Vec<(u64, u32, u32)> {
        if self.window_resize_in_progress {
            return Vec::new();
        }
        let scale = self.window.scale_factor().max(f64::EPSILON);
        self.current_windows
            .iter()
            .filter_map(|window| {
                let [_, _, width, height] = window.rect;
                if width <= 0 || height <= 0 {
                    return None;
                }
                let width = ((f64::from(width) / scale).round() as u32).max(1);
                let height = ((f64::from(height) / scale).round() as u32).max(1);
                (self.sent_managed_sizes.get(&window.window_id) != Some(&(width, height)))
                    .then_some((window.window_id, width, height))
            })
            .collect()
    }

    fn mark_managed_resize_sent(&mut self, window: u64, width: u32, height: u32) {
        self.sent_managed_sizes.insert(window, (width, height));
    }
}

fn plugin_pane_is_visible(
    realm: Option<(BackendId, u64)>,
    active_backend: Option<BackendId>,
    active_session: u64,
) -> bool {
    realm.is_none_or(|(backend, session)| {
        active_backend == Some(backend) && active_session == session
    })
}

fn visible_plugin_panes_for_realm(
    pane_realms: &BTreeMap<u64, Option<(BackendId, u64)>>,
    active_backend: Option<BackendId>,
    active_session: u64,
) -> Vec<u64> {
    pane_realms
        .iter()
        .filter_map(|(pane, realm)| {
            plugin_pane_is_visible(*realm, active_backend, active_session).then_some(*pane)
        })
        .collect()
}

fn pane_inventory_from_catalogs(
    plugin_pane_realms: &BTreeMap<u64, Option<(BackendId, u64)>>,
    available_windows: &[AvailableWindow],
) -> Vec<u64> {
    plugin_pane_realms
        .keys()
        .copied()
        .chain(available_windows.iter().map(|window| window.pane))
        .collect()
}

fn newly_declared_global_plugin_panes(
    previously_owned: &HashSet<u64>,
    inventory: &[u64],
    plugin_pane_realms: &BTreeMap<u64, Option<(BackendId, u64)>>,
) -> Vec<u64> {
    inventory
        .iter()
        .copied()
        .filter(|pane| {
            !previously_owned.contains(pane)
                && plugin_pane_realms.get(pane).is_some_and(Option::is_none)
        })
        .collect()
}

#[cfg(test)]
fn default_visible_panes(
    plugin_panes: impl IntoIterator<Item = u64>,
    windows: &[AvailableWindow],
    active_backend: BackendId,
    active_session: u64,
) -> Vec<u64> {
    plugin_panes
        .into_iter()
        .chain(
            windows
                .iter()
                .filter(|window| {
                    window.backend == active_backend && window.session == active_session
                })
                .map(|window| window.pane),
        )
        .collect()
}

const fn managed_pane_uses_placeholder(available: bool, frame_ready: bool) -> bool {
    available && !frame_ready
}

fn clip_rect_to_extent(rect: [i32; 4], extent: (u32, u32)) -> Option<[i32; 4]> {
    let extent_width = i32::try_from(extent.0).unwrap_or(i32::MAX);
    let extent_height = i32::try_from(extent.1).unwrap_or(i32::MAX);
    let [x, y, width, height] = rect;
    let x = x.clamp(0, extent_width);
    let y = y.clamp(0, extent_height);
    let width = width.max(0).min(extent_width - x);
    let height = height.max(0).min(extent_height - y);
    (width > 0 && height > 0).then_some([x, y, width, height])
}

fn scene_focus_target(
    current: Option<FocusedPane>,
    plugin_panes: impl IntoIterator<Item = u64>,
    managed_panes: &[u64],
    backend_focus: Option<u64>,
) -> Option<FocusedPane> {
    let plugin_panes = plugin_panes.into_iter().collect::<Vec<_>>();
    match current {
        Some(FocusedPane::Plugin(pane)) if plugin_panes.contains(&pane) => {
            Some(FocusedPane::Plugin(pane))
        }
        Some(FocusedPane::Managed(pane)) if managed_panes.contains(&pane) => {
            Some(FocusedPane::Managed(pane))
        }
        Some(FocusedPane::Plugin(_) | FocusedPane::Managed(_)) | None => backend_focus
            .filter(|pane| managed_panes.contains(pane))
            .or_else(|| managed_panes.first().copied())
            .map(FocusedPane::Managed)
            .or_else(|| plugin_panes.first().copied().map(FocusedPane::Plugin)),
    }
}

fn focus_request_target(
    accepted_layout: &ResolvedLayout,
    pane: u64,
    target: Option<FocusedPane>,
) -> Option<FocusedPane> {
    accepted_layout.pane_item(pane).and(target)
}

fn focus_change_between(
    previous: Option<FocusedPane>,
    target: Option<FocusedPane>,
) -> Option<FocusChange> {
    if matches!((previous, target), (None, None)) || previous == target {
        return None;
    }
    let plugin_blur = match previous {
        Some(FocusedPane::Plugin(pane)) if target != Some(FocusedPane::Plugin(pane)) => Some(pane),
        Some(FocusedPane::Plugin(_) | FocusedPane::Managed(_)) | None => None,
    };
    let plugin_focus = match target {
        Some(FocusedPane::Plugin(pane)) if previous != Some(FocusedPane::Plugin(pane)) => {
            Some(pane)
        }
        Some(FocusedPane::Plugin(_) | FocusedPane::Managed(_)) | None => None,
    };
    let managed_window = match target {
        Some(FocusedPane::Managed(window_id)) => Some(window_id),
        Some(FocusedPane::Plugin(_)) | None => None,
    };
    (plugin_blur.is_some() || plugin_focus.is_some() || managed_window.is_some()).then_some(
        FocusChange {
            plugin_blur,
            plugin_focus,
            managed_window,
            reset_keyboard: true,
        },
    )
}

fn project_point_between_rects(x: f64, y: f64, from: [i32; 4], to: [i32; 4]) -> (f64, f64) {
    let [from_x, from_y, from_width, from_height] = from;
    let [to_x, to_y, to_width, to_height] = to;
    let local_x = (x - f64::from(from_x)) / f64::from(from_width.max(1));
    let local_y = (y - f64::from(from_y)) / f64::from(from_height.max(1));
    (
        local_x.mul_add(f64::from(to_width.max(1)), f64::from(to_x)),
        local_y.mul_add(f64::from(to_height.max(1)), f64::from(to_y)),
    )
}

fn positive_rect_extent(rect: [i32; 4]) -> Option<(u32, u32)> {
    let width = u32::try_from(rect[2]).ok().filter(|width| *width > 0)?;
    let height = u32::try_from(rect[3]).ok().filter(|height| *height > 0)?;
    Some((width, height))
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "plugin pointer events use pane-local f32 logical coordinates"
)]
fn plugin_local_point(x: f64, y: f64, rect: [i32; 4], scale: f32) -> (f32, f32) {
    let (x, y) = project_point_between_rects(x, y, rect, [0, 0, rect[2], rect[3]]);
    (x as f32 / scale, y as f32 / scale)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "WGPU viewports require f32 while protocol and surface extents are integer pixels"
)]
fn scaled_viewport(
    rect: [i32; 4],
    scene_extent: (u32, u32),
    target_extent: (u32, u32),
) -> [f32; 4] {
    let [x, y, width, height] = rect;
    let scale_x = target_extent.0 as f32 / scene_extent.0 as f32;
    let scale_y = target_extent.1 as f32 / scene_extent.1 as f32;
    [
        x as f32 * scale_x,
        y as f32 * scale_y,
        width as f32 * scale_x,
        height as f32 * scale_y,
    ]
}

#[derive(Clone, Copy)]
struct RenderTimings {
    acquire_us: u128,
    encode_us: u128,
    submit_us: u128,
    present_us: u128,
}

#[derive(Clone, Copy)]
enum RenderOutcome {
    Presented {
        sequence: Option<u64>,
        reconfigure: bool,
        timings: RenderTimings,
    },
    Reconfigure,
    Skipped,
    Failed(&'static str),
}

fn low_latency_present_mode(supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    [
        wgpu::PresentMode::Mailbox,
        wgpu::PresentMode::Immediate,
        wgpu::PresentMode::FifoRelaxed,
        wgpu::PresentMode::Fifo,
    ]
    .into_iter()
    .find(|candidate| supported.contains(candidate))
    .unwrap_or(wgpu::PresentMode::AutoNoVsync)
}

#[cfg(test)]
mod tests {
    use super::*;
    use default_workspace_backend_layout::{PanePlacement, pane_placement};

    #[test]
    fn native_logical_characters_fill_missing_text_events() {
        let key = Key::Character("i".into());

        assert_eq!(
            plugin_text_input(&key, None, ModifiersState::empty()).as_deref(),
            Some("i")
        );
        assert_eq!(
            plugin_text_input(&key, Some("insert"), ModifiersState::empty()).as_deref(),
            Some("insert"),
            "an explicit text event remains authoritative and is not duplicated"
        );
    }

    #[test]
    fn native_text_fallback_does_not_turn_shortcuts_into_text() {
        let key = Key::Character("s".into());

        assert_eq!(plugin_text_input(&key, None, ModifiersState::CONTROL), None);
        assert_eq!(plugin_text_input(&key, None, ModifiersState::ALT), None);
        assert_eq!(
            plugin_text_input(&Key::Named(NamedKey::Enter), None, ModifiersState::empty()),
            None
        );
    }

    #[test]
    fn frontend_wayland_state_drops_before_the_event_loop_connection() {
        #[derive(Debug)]
        struct DropMarker {
            name: &'static str,
            order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>,
        }

        impl Drop for DropMarker {
            fn drop(&mut self) {
                self.order.borrow_mut().push(self.name);
            }
        }

        let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let marker = |name| {
            Some(DropMarker {
                name,
                order: std::rc::Rc::clone(&order),
            })
        };
        let mut wayland_drop = marker("drop-bridge");
        let mut renderer = marker("renderer-window");
        let mut clipboard = marker("clipboard");

        drop_frontend_wayland_state(&mut wayland_drop, &mut renderer, &mut clipboard);

        assert!(wayland_drop.is_none());
        assert!(renderer.is_none());
        assert!(clipboard.is_none());
        assert_eq!(
            order.borrow().as_slice(),
            ["drop-bridge", "renderer-window", "clipboard"]
        );
    }

    #[test]
    fn an_empty_catalogue_does_not_implicitly_create_or_bind_a_session() {
        let backend = BackendId(7);
        assert_eq!(bound_session_for_active(backend, 0), None);
        assert_eq!(bound_session_for_active(backend, 11), Some((backend, 11)));
        assert!(DEFAULT_WINDOW_WIDTH >= 1280.0);
        assert!(DEFAULT_WINDOW_HEIGHT >= 800.0);
    }

    #[test]
    fn latency_percentiles_use_nearest_rank_for_small_remote_samples() {
        let samples = (1_u128..=14).collect::<VecDeque<_>>();
        let summary = latency_summary(&samples, 14, 14, 14);
        assert_eq!(summary.get("p50_key_to_present_us"), Some(&7.into()));
        assert_eq!(summary.get("p95_key_to_present_us"), Some(&14.into()));
        assert_eq!(summary.get("p99_key_to_present_us"), Some(&14.into()));
    }

    #[test]
    fn native_resize_events_extend_one_settlement_transaction() {
        let started = Instant::now();
        let mut transaction = WindowResizeTransaction::new(started, (800, 600), (810, 610));
        for step in 1..12 {
            transaction.observe(
                started + Duration::from_millis(step * 16),
                (810 + step as u32, 610 + step as u32),
            );
        }

        assert_eq!(transaction.events, 12);
        assert_eq!(transaction.initial_size, (800, 600));
        assert_eq!(transaction.latest_size, (821, 621));
        assert!(!transaction.ready(started + Duration::from_millis(295)));
        assert!(transaction.ready(started + Duration::from_millis(296)));
    }

    #[test]
    fn terminal_trace_sequences_are_carried_by_input_and_snapshot_messages() {
        let mut input = vec![b'I'];
        input.extend_from_slice(&17_u64.to_le_bytes());
        input.push(b'a');
        assert_eq!(terminal_input_sequence(&input), Some(17));

        let mut snapshot = vec![0_u8; 41];
        snapshot[..4].copy_from_slice(b"BTRM");
        snapshot[4] = 4;
        snapshot[14..22].copy_from_slice(&17_u64.to_le_bytes());
        snapshot[37..39].copy_from_slice(&1_u16.to_le_bytes());
        snapshot[39..41].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(terminal_snapshot_input_sequence(&snapshot), Some(17));
        assert!(terminal_snapshot_completes_revision(&snapshot));

        snapshot[37..39].copy_from_slice(&0_u16.to_le_bytes());
        assert!(!terminal_snapshot_completes_revision(&snapshot));
    }

    #[test]
    fn latency_stages_partition_one_key_to_present_sample() {
        let started = Instant::now();
        let queued = started + Duration::from_micros(1);
        let sent = queued + Duration::from_micros(2);
        let transport = sent + Duration::from_micros(5);
        let snapshot = transport + Duration::from_micros(2);
        let graphics = snapshot + Duration::from_micros(3);
        let presented = graphics + Duration::from_micros(5);
        let mut stages = PluginLatencyStages::default();
        stages.record(
            started,
            Some(queued),
            Some(sent),
            transport,
            snapshot,
            graphics,
            presented,
        );
        assert_eq!(stages.input_to_transport_queue, VecDeque::from([1]));
        assert_eq!(stages.transport_send_queue, VecDeque::from([2]));
        assert_eq!(stages.remote_round_trip, VecDeque::from([5]));
        assert_eq!(stages.transport_round_trip, VecDeque::from([8]));
        assert_eq!(stages.client_dispatch, VecDeque::from([2]));
        assert_eq!(stages.plugin_render, VecDeque::from([3]));
        assert_eq!(stages.presentation, VecDeque::from([5]));
    }

    fn available(pane: u64, session: u64) -> AvailableWindow {
        AvailableWindow {
            backend: BackendId(1),
            pane,
            session,
            app_id: format!("app-{pane}"),
        }
    }

    fn assert_placement_close(actual: PanePlacement, expected: PanePlacement) {
        let epsilon = 0.000_1;
        assert!((actual.offset_u - expected.offset_u).abs() < epsilon);
        assert!((actual.offset_v - expected.offset_v).abs() < epsilon);
        assert!((actual.size_u - expected.size_u).abs() < epsilon);
        assert!((actual.size_v - expected.size_v).abs() < epsilon);
    }

    #[test]
    fn layout_separates_backend_groups_into_weighted_columns() {
        let panes = [10, 20, 11];
        let backends = [1, 2, 1];
        assert_placement_close(
            pane_placement(0, &panes, &backends, 1200, 800),
            PanePlacement {
                offset_u: -1.0,
                offset_v: -1.0,
                size_u: 4.0 / 3.0,
                size_v: 1.0,
            },
        );
        assert_placement_close(
            pane_placement(1, &panes, &backends, 1200, 800),
            PanePlacement {
                offset_u: 1.0 / 3.0,
                offset_v: -1.0,
                size_u: 2.0 / 3.0,
                size_v: 2.0,
            },
        );
        assert_placement_close(
            pane_placement(2, &panes, &backends, 1200, 800),
            PanePlacement {
                offset_u: -1.0,
                offset_v: 0.0,
                size_u: 4.0 / 3.0,
                size_v: 1.0,
            },
        );
    }

    #[test]
    fn one_backend_uses_an_aspect_aware_balanced_grid() {
        let panes = [10, 11, 12];
        let backends = [7, 7, 7];
        assert_placement_close(
            pane_placement(0, &panes, &backends, 1600, 900),
            PanePlacement {
                offset_u: -1.0,
                offset_v: -1.0,
                size_u: 1.0,
                size_v: 1.0,
            },
        );
        assert_placement_close(
            pane_placement(2, &panes, &backends, 1600, 900),
            PanePlacement {
                offset_u: -1.0,
                offset_v: 0.0,
                size_u: 2.0,
                size_v: 1.0,
            },
        );
    }

    #[test]
    fn workspace_layout_keeps_sixty_four_windows_inside_the_viewport() {
        let panes = (0..64).collect::<Vec<_>>();
        let backends = panes.iter().map(|pane| pane / 16).collect::<Vec<_>>();
        let placements = panes
            .iter()
            .enumerate()
            .map(|(index, _)| pane_placement(index, &panes, &backends, 1920, 1080))
            .collect::<Vec<_>>();

        for placement in placements {
            assert!(placement.offset_u >= -1.0);
            assert!(placement.offset_v >= -1.0);
            assert!(placement.offset_u + placement.size_u <= 1.000_1);
            assert!(placement.offset_v + placement.size_v <= 1.000_1);
            assert!(placement.size_u > 0.0);
            assert!(placement.size_v > 0.0);
        }
    }

    #[test]
    fn layout_state_qualifies_managed_and_plugin_panes_by_backend() {
        let windows = vec![AvailableWindow {
            backend: BackendId(1),
            pane: 10,
            session: 4,
            app_id: "managed".to_string(),
        }];
        let plugin_realms = BTreeMap::from([
            (20, Some((BackendId(2), 7))),
            (30, None),
            (40, Some((BackendId(3), 9))),
        ]);

        assert_eq!(
            layout_pane_realms(&windows, &plugin_realms, &[10, 20, 30]),
            [
                LayoutPaneRealm {
                    pane: 10,
                    backend: BackendId(1),
                    session: 4,
                },
                LayoutPaneRealm {
                    pane: 20,
                    backend: BackendId(2),
                    session: 7,
                },
            ]
        );
    }

    #[test]
    fn session_selection_filters_default_view_without_reducing_layout_authority() {
        let mut windows = vec![available(10, 1), available(20, 2), available(30, 2)];
        windows[2].backend = BackendId(2);
        assert_eq!(
            default_visible_panes([u64::MAX], &windows, BackendId(1), 1),
            [u64::MAX, 10]
        );
        assert_eq!(
            default_visible_panes([u64::MAX], &windows, BackendId(1), 2),
            [u64::MAX, 20]
        );
        assert_eq!(
            default_visible_panes([u64::MAX], &windows, BackendId(2), 2),
            [u64::MAX, 30]
        );

        let inventory = windows.iter().map(|window| window.pane).collect::<Vec<_>>();
        let inactive_session_layout = native_flat_layout(&[20, 30], LayoutSize { w: 1280, h: 800 });
        assert!(
            inactive_session_layout
                .validate_for_panes(&inventory)
                .is_ok(),
            "session selection must not constrain which available windows an actor can map"
        );
    }

    #[test]
    fn plugin_panes_keep_their_declared_session_scope() {
        assert!(plugin_pane_is_visible(None, Some(BackendId(1)), 7));
        assert!(plugin_pane_is_visible(
            Some((BackendId(1), 7)),
            Some(BackendId(1)),
            7,
        ));
        assert!(!plugin_pane_is_visible(
            Some((BackendId(1), 7)),
            Some(BackendId(1)),
            8,
        ));
        assert!(!plugin_pane_is_visible(
            Some((BackendId(1), 7)),
            Some(BackendId(2)),
            7,
        ));
        assert!(!plugin_pane_is_visible(Some((BackendId(1), 7)), None, 0,));
    }

    #[test]
    fn declared_plugin_panes_are_visible_before_their_first_graphics_packet() {
        let panes = BTreeMap::from([
            (11, None),
            (12, Some((BackendId(1), 7))),
            (13, Some((BackendId(1), 8))),
        ]);

        assert_eq!(
            visible_plugin_panes_for_realm(&panes, Some(BackendId(1)), 7),
            [11, 12]
        );
        let inventory = pane_inventory_from_catalogs(&panes, &[]);
        assert_eq!(inventory, [11, 12, 13]);
        assert!(
            native_flat_layout(&[11], LayoutSize { w: 1280, h: 800 })
                .validate_for_panes(&inventory)
                .is_ok()
        );
    }

    #[test]
    fn focus_of_a_declared_but_not_yet_composed_pane_stays_deferred() {
        let old_layout = native_flat_layout(&[11], LayoutSize { w: 1280, h: 800 });
        assert_eq!(
            focus_request_target(&old_layout, 12, Some(FocusedPane::Plugin(12))),
            None,
            "declaring a pane must not let it steal focus before layout composition"
        );

        let replacement = native_flat_layout(&[11, 12], LayoutSize { w: 1280, h: 800 });
        assert_eq!(
            focus_request_target(&replacement, 12, Some(FocusedPane::Plugin(12))),
            Some(FocusedPane::Plugin(12)),
            "the pending focus target becomes usable with the accepted replacement"
        );
    }

    #[test]
    fn late_global_shell_panes_remain_owned_by_the_root_layout_lease() -> Result<()> {
        let mut leases = LayoutLeaseTree::default();
        leases.update_inventory(1, &[1, 2])?;
        let root = leases.root_token();
        let rest = leases.grant_rest(root, &[2])?;
        let previously_owned = leases
            .lease_tokens()
            .into_iter()
            .flat_map(|token| leases.owned_panes(token).unwrap_or_default())
            .collect::<HashSet<_>>();
        let pane_realms = BTreeMap::from([(1, None), (3, None), (4, Some((BackendId(1), 7)))]);
        let inventory = [1, 2, 3, 4];
        let new_global =
            newly_declared_global_plugin_panes(&previously_owned, &inventory, &pane_realms);

        leases.update_inventory(2, &inventory)?;
        leases.assign_panes(root, &new_global)?;

        assert_eq!(new_global, [3]);
        assert_eq!(leases.owned_panes(root), Some(vec![1, 3]));
        assert_eq!(leases.owned_panes(rest), Some(vec![2, 4]));
        Ok(())
    }

    #[test]
    fn mapping_readiness_is_not_part_of_default_layout_input() {
        let windows = vec![available(41, 7), available(42, 8)];
        assert_eq!(default_visible_panes([], &windows, BackendId(1), 7), [41]);
        assert!(managed_pane_uses_placeholder(true, false));
        assert!(!managed_pane_uses_placeholder(true, true));
        assert!(!managed_pane_uses_placeholder(false, false));
    }

    #[test]
    fn input_translation_targets_backend_local_panes() {
        let mut message = ClientMessage::Input(InputEvent::PointerMotion {
            position: PanePosition {
                pane: 900,
                x: 1.0,
                y: 2.0,
            },
        });
        let routes = BTreeMap::from([(900, 7)]);
        translate_input_panes(&mut message, &routes);
        assert!(matches!(
            message,
            ClientMessage::Input(InputEvent::PointerMotion {
                position: PanePosition { pane: 7, .. }
            })
        ));
    }

    #[test]
    fn disconnecting_one_backend_keeps_other_input_routes_live() {
        let first = BackendId(1);
        let second = BackendId(2);
        let empty_lane = || InputLane::Multi {
            lanes: BTreeMap::new(),
            routes: BTreeMap::new(),
            default: None,
        };
        let mut lane = InputLane::Multi {
            lanes: BTreeMap::from([
                (first, Box::new(empty_lane())),
                (second, Box::new(empty_lane())),
            ]),
            routes: BTreeMap::from([(10, first), (20, second)]),
            default: Some(first),
        };

        assert_eq!(lane.backend_count(), 2);
        assert!(lane.remove_backend(first));
        if let InputLane::Multi {
            lanes,
            routes,
            default,
        } = lane
        {
            assert_eq!(lanes.keys().copied().collect::<Vec<_>>(), [second]);
            assert_eq!(routes, BTreeMap::from([(20, second)]));
            assert_eq!(default, Some(second));
        } else {
            assert!(false, "test lane stopped being multi-backend");
        }
    }

    #[test]
    fn remote_failure_detaches_only_that_backend() {
        let local = BackendId(1);
        let remote = BackendId(2);
        let input_leaf = || InputLane::Multi {
            lanes: BTreeMap::new(),
            routes: BTreeMap::new(),
            default: None,
        };
        let actor_leaf = || ActorMessageLane::Multi {
            lanes: BTreeMap::new(),
            default: None,
        };
        let command_leaf = || BackendCommandLane::Multi {
            lanes: BTreeMap::new(),
            default: None,
        };
        let mut application = Application {
            input_sender: Some(InputLane::Multi {
                lanes: BTreeMap::from([
                    (local, Box::new(input_leaf())),
                    (remote, Box::new(input_leaf())),
                ]),
                routes: BTreeMap::from([(10, local), (20, remote)]),
                default: Some(remote),
            }),
            actor_message_sender: Some(ActorMessageLane::Multi {
                lanes: BTreeMap::from([
                    (local, Box::new(actor_leaf())),
                    (remote, Box::new(actor_leaf())),
                ]),
                default: Some(remote),
            }),
            backend_commands: Some(BackendCommandLane::Multi {
                lanes: BTreeMap::from([
                    (local, Box::new(command_leaf())),
                    (remote, Box::new(command_leaf())),
                ]),
                default: Some(remote),
            }),
            ..Application::default()
        };
        application.backend_registry.replace_panes(local, None, [1]);
        application
            .backend_registry
            .replace_panes(remote, None, [1]);
        let local_view = application.backend_registry.view_pane(QualifiedPane {
            backend: local,
            pane: 1,
        });
        application.backend_registry.view_pane(QualifiedPane {
            backend: remote,
            pane: 1,
        });
        application.backend_sessions.insert(
            remote,
            SessionSnapshot {
                backend_id: remote.0,
                revision: 1,
                active: 1,
                sessions: Vec::new(),
            },
        );

        assert!(
            application
                .disconnect_remote_backend(remote, "test disconnect")
                .is_ok()
        );
        assert_eq!(
            application
                .input_sender
                .as_ref()
                .map(InputLane::backend_count),
            Some(1)
        );
        assert!(!application.backend_sessions.contains_key(&remote));
        assert_eq!(
            application.backend_registry.all_view_routes(),
            vec![(local_view, local, 1)]
        );
    }

    #[test]
    fn image_mime_detection_uses_bytes_instead_of_file_names() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xffrest"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF1234WEBPrest"), Some("image/webp"));
        assert_eq!(image_mime_type(b"not an image"), None);
    }

    #[test]
    fn image_preview_preserves_aspect_ratio_and_pixels() {
        let source = image::RgbaImage::from_pixel(192, 128, image::Rgba([255, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let preview = decode_agent_image_preview(encoded.get_ref()).unwrap();
        assert_eq!((preview.width, preview.height), (120, 80));
        assert_eq!(preview.rgba8.get(0..4), Some([255, 0, 0, 255].as_slice()));
    }
}
