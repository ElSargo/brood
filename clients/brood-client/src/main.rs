#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bounded window, text, and graphics coordinates intentionally narrow to the f32 and integer formats used by WGPU"
)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    mem::size_of,
    os::fd::AsFd,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use brood_cli::{
    ClientArgs,
    control::{
        ControlTarget, DmabufFrame, PresentationWindowSummary, ServerLaunch,
        activate_surface_session, close_surface_session, connect_or_spawn,
        create_managed_worktree_session, create_surface_session, dmabuf_resource,
        open_presentation_fast_path, rename_surface_session, request_dbus_command,
        resolve_control_target,
    },
};
use brood_dmabuf::{SinglePlaneDmabuf, import_single_plane};
use brood_layout::{
    LayoutAction, LayoutItemKind, LayoutLeaseId, LayoutLeaseTree, LayoutRect, LayoutSize,
    LocalPoint, ResolvedLayout,
};
use clap::Parser;
use num_traits::ToPrimitive;
use serde::Deserialize;
use wgpu::{Color, CurrentSurfaceTexture};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, ModifiersState, NamedKey},
    platform::scancode::PhysicalKeyExtScancode,
    window::{UserAttentionType, Window, WindowId},
};

mod actor_runtime;
mod key_policy;
mod layout;
mod plugin_gpu;
mod plugin_graphics;

use actor_runtime::{ActorIdentity, PresentationWasmRuntime};
use brood_wasm::{
    GlobalKeyDecision, HostMessage, PresentationEvent, SessionCommandCompletion,
    SessionCommandRequest, SessionEvent, SessionOperation, SessionSnapshot, SessionSummary,
    WasmActorEventKind, WasmPluginArtifact,
};
use key_policy::{ClientKeyPolicy, KeyPolicyAppPane, KeyPolicySnapshot};
use layout::{
    LayoutSource, convert_actor_layout_fragment, convert_actor_layout_lease, native_flat_layout,
};
use plugin_gpu::{GpuCanvasRenderer, GpuCanvasScene};
use plugin_graphics::PluginGraphicsBridge;

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
            Some(1),
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
    let mut key_policy = ClientKeyPolicy::load(key_policy_spec)?;
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
        let new_agent_ui = key_policy.evaluate(
            38,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            false,
            &snapshot,
        )?;
        let repeated_new_agent_ui = key_policy.evaluate(
            38,
            brood_wasm::ABI_KEY_PRESSED,
            brood_wasm::ABI_MOD_CTRL | brood_wasm::ABI_MOD_ALT,
            true,
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
                message.actor == "session-switcher"
                    && message.payload == br#"{"type":"sidebar-action","action":"toggle"}"#
            })
            || !new_agent_ui.actor_messages.iter().any(|message| {
                message.actor == "brood.client-init"
                    && message.payload == br#"{"kind":"coding-agent-action","action":"new-ui"}"#
            })
            || !repeated_new_agent_ui.actor_messages.is_empty()
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
    let (
        input_sender,
        actor_message_sender,
        backend_commands,
        fast_path_enabled,
        backend_connected,
        bound_session_id,
    ) = if args.no_server {
        eprintln!("starting Brood presentation without a backend");
        (None, None, None, false, false, None)
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
        let fast_path = open_presentation_fast_path(&target)?;
        let session_id = if fast_path.active_session != 0 {
            eprintln!(
                "attached presentation to active session id={}",
                fast_path.active_session
            );
            fast_path.active_session
        } else {
            let created = create_surface_session(&target, String::new())?;
            eprintln!("created presentation session id={created}");
            created
        };

        let fast_path_enabled = true;
        let (input_sender, actor_message_sender) = if fast_path_enabled {
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
                event_loop.create_proxy(),
            )?;
            spawn_catalog_updates(catalog, catalog_notifications, event_loop.create_proxy())?;
            spawn_actor_message_updates(
                brood_ipc::ActorMessageConsumer::map(
                    fast_path.presentation_message_fd.as_fd(),
                    fast_path.generation,
                )?,
                brood_ipc::EventNotifier::from_owned_fd(fast_path.presentation_message_notify_fd),
                event_loop.create_proxy(),
            )?;
            let input_sender = InputLane {
                producer: input,
                notifications: brood_ipc::EventNotifier::from_owned_fd(fast_path.input_notify_fd),
                generation: fast_path.generation,
                sequence: 0,
                deferred: VecDeque::new(),
                deferred_high_water: 0,
                coalesced_motion: 0,
            };
            let actor_message_sender = ActorMessageLane {
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
            (Some(input_sender), Some(actor_message_sender))
        } else {
            (None, None)
        };
        let backend_commands = Some(BackendCommandLane::start(
            target,
            event_loop.create_proxy(),
        )?);
        (
            input_sender,
            actor_message_sender,
            backend_commands,
            fast_path_enabled,
            true,
            Some(session_id),
        )
    };
    let mut application = Application {
        wasm_runtime: Some(PresentationWasmRuntime::start(
            &wasm_actor_specs,
            init_actor.as_deref(),
            bound_session_id,
            Some(&plugin_graphics),
            Some(event_proxy.clone()),
        )?),
        key_policy: Some(key_policy),
        plugin_graphics: Some(plugin_graphics),
        input_sender,
        actor_message_sender,
        backend_commands,
        fast_path_enabled,
        backend_connected,
        window_focused: true,
        event_proxy: Some(event_proxy),
        ..Application::default()
    };

    let run_result = event_loop
        .run_app(&mut application)
        .context("run event loop");

    if let Some(sender) = application.input_sender.as_mut() {
        let _ = send_input(sender, brood_ipc::INPUT_CLIENT_DISCONNECT, [0, 0, 0, 0, 0]);
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
    eprintln!(
        "plugin input diagnostics {{\"key_to_present_samples\":{},\"last_key_to_present_us\":{},\"max_key_to_present_us\":{}}}",
        application.plugin_key_to_present_samples,
        application.plugin_last_key_to_present_us,
        application.plugin_max_key_to_present_us,
    );
    let backend_shutdown_result = application
        .backend_commands
        .as_mut()
        .map_or(Ok(()), BackendCommandLane::shutdown)
        .context("stop presentation backend command worker");
    let error = application.error.take();
    drop(application);
    run_result?;
    shutdown_result?;
    backend_shutdown_result?;
    if let Some(error) = error {
        return Err(error);
    }

    Ok(())
}

#[derive(Debug)]
enum ClientEvent {
    SceneFrame {
        sequence: u64,
        width: u32,
        height: u32,
        windows: Vec<SceneWindowUpdate>,
    },
    FocusPresentation,
    ClosePresentation,
    TransportError(String),
    PluginGraphicsChanged {
        pane: u64,
        sequence: u64,
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
        session: Option<SessionSnapshot>,
        active_session: u64,
        windows: Vec<PresentationWindowSummary>,
    },
    SessionCommandCompleted {
        actor_id: String,
        completion: SessionCommandCompletion,
    },
    ActorHostMessagesReady,
    SessionActorMessage {
        session_id: u64,
        sender: String,
        recipient: String,
        body: Vec<u8>,
    },
}

#[derive(Debug)]
struct SceneWindowUpdate {
    window_id: u64,
    resource_id: u64,
    rect: [i32; 4],
    active: bool,
    resource: Option<DmabufFrame>,
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
                                    session: Some(SessionSnapshot {
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
                                    Ok(resource) => Some(resource),
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
                            ClientEvent::SessionActorMessage {
                                session_id: message.session_id(),
                                sender: message.sender()?.to_string(),
                                recipient: message.recipient()?.to_string(),
                                body: message.body()?.to_vec(),
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

struct InputLane {
    producer: brood_ipc::CriticalProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
    deferred: VecDeque<brood_ipc::CriticalRecord>,
    deferred_high_water: usize,
    coalesced_motion: u64,
}

struct ActorMessageLane {
    producer: brood_ipc::ActorMessageProducer,
    notifications: brood_ipc::EventNotifier,
    generation: u64,
    sequence: u64,
    deferred: VecDeque<Box<brood_ipc::ActorMessageRecord>>,
}

impl ActorMessageLane {
    fn send(&mut self, message: &actor_runtime::SessionActorMessageRequest) -> Result<()> {
        self.flush_deferred();
        self.sequence = self.sequence.wrapping_add(1);
        let record = brood_ipc::ActorMessageRecord::new(
            self.generation,
            self.sequence,
            message.session_id,
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

impl InputLane {
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
        if record.opcode == brood_ipc::INPUT_WINDOW_POINTER_MOTION
            && let Some(previous) = self.deferred.back_mut()
            && previous.opcode == brood_ipc::INPUT_WINDOW_POINTER_MOTION
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

struct BackendCommandLane {
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

impl BackendCommandLane {
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

fn execute_session_command(
    target: &ControlTarget,
    request: SessionCommandRequest,
) -> SessionCommandCompletion {
    let request_id = request.request;
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
        SessionOperation::Create => {
            create_surface_session(target, name.unwrap_or_default()).map(Some)
        }
        SessionOperation::CreateWorktree => session
            .context("create worktree request is missing a source session ID")
            .and_then(|session| {
                let branch = name.context("create worktree request is missing a branch name")?;
                create_managed_worktree_session(target, session, branch).map(Some)
            }),
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
            request: request_id,
            operation,
            success: true,
            session,
            message: String::new(),
        },
        Err(error) => SessionCommandCompletion {
            request: request_id,
            operation,
            success: false,
            session,
            message: format!("{error:#}"),
        },
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
    pending_frame: Option<ClientEvent>,
    pending_backend_snapshot: Option<(u64, Vec<PresentationWindowSummary>)>,
    latest_session_snapshot: Option<SessionSnapshot>,
    last_presented_sequence: Option<u64>,
    modifiers: ModifiersState,
    window_focused: bool,
    cursor_position: Option<(f64, f64)>,
    pending_plugin_pointer_motion: Option<(u64, f32, f32)>,
    managed_pointer_target: Option<u64>,
    pointer_grab: Option<PointerGrab>,
    wasm_runtime: Option<PresentationWasmRuntime>,
    key_policy: Option<ClientKeyPolicy>,
    plugin_graphics: Option<PluginGraphicsBridge>,
    backend_commands: Option<BackendCommandLane>,
    event_proxy: Option<winit::event_loop::EventLoopProxy<ClientEvent>>,
    next_renderer_generation: u64,
    renderer_recoveries: u8,
    layout_revision: u64,
    layout_leases: LayoutLeaseTree,
    root_layout_actor: Option<ActorIdentity>,
    layout_holders: BTreeMap<LayoutLeaseId, ActorIdentity>,
    layout_publications: BTreeMap<LayoutLeaseId, u64>,
    pending_plugin_input: Option<Instant>,
    pending_plugin_scene: bool,
    plugin_key_to_present_samples: u64,
    plugin_last_key_to_present_us: u128,
    plugin_max_key_to_present_us: u128,
}

impl Application {
    fn accept_frame(&mut self, event: ClientEvent) -> Result<()> {
        let (sequence, width, height, windows) = match event {
            ClientEvent::SceneFrame {
                sequence,
                width,
                height,
                windows,
            } => (sequence, width, height, windows),
            ClientEvent::TransportError(message) => return Err(anyhow!(message)),
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
            ClientEvent::ActorHostMessagesReady => {
                return Err(anyhow!("actor host message reached scene transport path"));
            }
            ClientEvent::SessionActorMessage { .. } => {
                return Err(anyhow!(
                    "session actor message reached scene transport path"
                ));
            }
        };
        let Some(renderer) = self.renderer.as_mut() else {
            self.pending_frame = Some(ClientEvent::SceneFrame {
                sequence,
                width,
                height,
                windows,
            });
            return Ok(());
        };
        let focus_change = renderer.select_scene(sequence, width, height, windows)?;
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
                &mut self.pending_plugin_pointer_motion,
                &mut self.managed_pointer_target,
                &mut self.pointer_grab,
                modifier_bits(self.modifiers),
            );
            route_pointer_motion(
                renderer,
                self.cursor_position,
                &mut self.pointer_grab,
                self.input_sender.as_mut(),
                &mut self.pending_plugin_pointer_motion,
                &mut self.managed_pointer_target,
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
            self.layout_leases
                .update_inventory(self.layout_revision, &panes)?;
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
                message => {
                    let Some(renderer) = self.renderer.as_mut() else {
                        continue;
                    };
                    if let Err(error) = apply_key_policy_host_message(
                        renderer,
                        &mut self.input_sender,
                        self.backend_commands.as_ref(),
                        self.wasm_runtime.as_ref(),
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
        let result =
            (|| {
                let Some(runtime) = self.wasm_runtime.as_mut() else {
                    return Ok(());
                };
                runtime.drain_events()?;
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
                let messages = runtime.drain_session_messages();
                let commands = runtime.drain_session_commands();
                if !commands.is_empty() {
                    let lane = self.backend_commands.as_ref().context(
                        "presentation actor requested session control without a backend",
                    )?;
                    for command in commands {
                        lane.try_send_session(command)?;
                    }
                }
                if !messages.is_empty() {
                    let lane = self.actor_message_sender.as_mut().context(
                        "presentation actor requested a session message without a backend",
                    )?;
                    for message in &messages {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                            eprintln!(
                                "presentation actor message sender={} recipient={} body={}",
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
            .create_window(Window::default_attributes().with_title(title))
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
                    proxy,
                )
            });

        match result {
            Ok(renderer) => {
                renderer.window.request_redraw();
                self.renderer = Some(renderer);
                if let Some((active_session, windows)) = self.pending_backend_snapshot.take()
                    && let Some(renderer) = self.renderer.as_mut()
                {
                    let _focus_change = renderer.select_backend_snapshot(active_session, windows);
                }
                if let (Some(sender), Some(renderer)) =
                    (self.input_sender.as_mut(), self.renderer.as_ref())
                {
                    send_presentation_configuration(sender, renderer);
                }
                if let Some(frame) = self.pending_frame.take()
                    && let Err(error) = self.accept_frame(frame)
                {
                    self.error = Some(error);
                    event_loop.exit();
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
            ClientEvent::FocusPresentation => {
                if let Some(renderer) = self.renderer.as_ref() {
                    renderer
                        .window
                        .request_user_attention(Some(UserAttentionType::Critical));
                    renderer.window.focus_window();
                }
            }
            ClientEvent::ClosePresentation => event_loop.exit(),
            ClientEvent::PluginGraphicsChanged { pane, sequence } => {
                let mut failure = None;
                if let Some(renderer) = self.renderer.as_mut() {
                    match renderer.refresh_plugin_graphics(pane, sequence) {
                        Ok(change) => {
                            if self.pending_plugin_input.is_some() {
                                self.pending_plugin_scene = true;
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
                session,
                active_session,
                windows,
            } => {
                let focus_change = if let Some(renderer) = self.renderer.as_mut() {
                    renderer.select_backend_snapshot(active_session, windows)
                } else {
                    self.pending_backend_snapshot = Some((active_session, windows));
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
                    self.latest_session_snapshot = Some(snapshot.clone());
                    if let Some(runtime) = self.wasm_runtime.as_mut() {
                        runtime.bind_session(snapshot.active);
                        if let Err(error) =
                            runtime.broadcast_session_event(&SessionEvent::Snapshot(snapshot))
                        {
                            self.fail(event_loop, error);
                        }
                    }
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
                            SessionOperation::Activate
                                | SessionOperation::Create
                                | SessionOperation::CreateWorktree
                        )
                    })
                    .and(completion.session)
                    .and_then(|session| {
                        self.renderer
                            .as_mut()
                            .and_then(|renderer| renderer.select_active_session(session))
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
                    && let Some(snapshot) = self.latest_session_snapshot.clone()
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
            ClientEvent::SessionActorMessage {
                session_id,
                sender,
                recipient,
                body,
            } => {
                if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                    if body.starts_with(b"BTRM") {
                        eprintln!(
                            "session actor message sender={sender} recipient={recipient} terminal_snapshot_bytes={}",
                            body.len()
                        );
                    } else {
                        eprintln!(
                            "session actor message sender={sender} recipient={recipient} body={}",
                            String::from_utf8_lossy(&body)
                        );
                    }
                }
                let expected_session = self
                    .wasm_runtime
                    .as_ref()
                    .and_then(PresentationWasmRuntime::bound_session_id);
                if expected_session != Some(session_id) {
                    eprintln!(
                        "dropped session actor message for session {session_id}; \
                         presentation is attached to {expected_session:?}"
                    );
                } else if let Some(runtime) = self.wasm_runtime.as_ref()
                    && let Err(error) =
                        runtime.send_actor_message(&recipient, format!("session:{sender}"), body)
                {
                    eprintln!(
                        "dropped session actor message for unavailable presentation actor \
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
        if let Err(error) = self.publish_layout_state() {
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
            &mut self.pending_plugin_pointer_motion,
        );
        if input_backpressured || actor_message_backpressured {
            // The shared-memory consumer already has a wakeup for the records
            // which filled its ring. Poll until it frees a slot so input state
            // transitions cannot remain deferred indefinitely.
            event_loop.set_control_flow(ControlFlow::Poll);
        } else if self
            .wasm_runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.is_empty())
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(50),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
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
                let focus_change = match renderer.resize(size.width, size.height) {
                    Ok(change) => change,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
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
                if let Err(error) =
                    flush_renderer_plugin_resize_events(self.wasm_runtime.as_ref(), renderer)
                {
                    self.fail(event_loop, error);
                    return;
                }
                if let Some(sender) = self.input_sender.as_mut() {
                    send_presentation_configuration(sender, renderer);
                }
                flush_renderer_managed_resize_requests(self.input_sender.as_mut(), renderer);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = Some((position.x, position.y));
                route_pointer_motion(
                    renderer,
                    self.cursor_position,
                    &mut self.pointer_grab,
                    self.input_sender.as_mut(),
                    &mut self.pending_plugin_pointer_motion,
                    &mut self.managed_pointer_target,
                );
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_position = None;
                self.pending_plugin_pointer_motion = None;
                send_managed_pointer_leave(
                    self.input_sender.as_mut(),
                    &mut self.managed_pointer_target,
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
                    &mut self.pending_plugin_pointer_motion,
                    &mut self.managed_pointer_target,
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
                        self.input_sender.as_mut(),
                        &mut self.pending_plugin_pointer_motion,
                        &mut self.managed_pointer_target,
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
                            &mut self.managed_pointer_target,
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
                        self.pending_plugin_pointer_motion = None;
                        if let Some(sender) = self.input_sender.as_mut() {
                            let sent = match delta {
                                MouseScrollDelta::LineDelta(horizontal, vertical) => send_input(
                                    sender,
                                    brood_ipc::INPUT_WINDOW_POINTER_WHEEL,
                                    [
                                        pane,
                                        brood_ipc::encode_i32_argument(wheel_v120(horizontal)),
                                        brood_ipc::encode_i32_argument(wheel_v120(vertical)),
                                        x.to_bits(),
                                        y.to_bits(),
                                    ],
                                ),
                                MouseScrollDelta::PixelDelta(position) => {
                                    let scale = renderer.window.scale_factor().max(f64::EPSILON);
                                    send_input(
                                        sender,
                                        brood_ipc::INPUT_WINDOW_POINTER_AXIS,
                                        [
                                            pane,
                                            (-position.x / scale).to_bits(),
                                            (-position.y / scale).to_bits(),
                                            x.to_bits(),
                                            y.to_bits(),
                                        ],
                                    )
                                }
                            };
                            if sent {
                                self.managed_pointer_target = Some(pane);
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
                        brood_ipc::INPUT_PRESENTATION_FOCUS,
                        [u64::from(focused), 0, 0, 0, 0],
                    );
                }
                if !focused {
                    if let Some(sender) = self.input_sender.as_mut() {
                        send_keyboard_reset(sender);
                    }
                    cancel_pointer_grab(
                        self.wasm_runtime.as_ref(),
                        self.input_sender.as_mut(),
                        &mut self.pending_plugin_pointer_motion,
                        &mut self.managed_pointer_target,
                        &mut self.pointer_grab,
                        modifier_bits(self.modifiers),
                    );
                    self.cursor_position = None;
                    self.pending_plugin_pointer_motion = None;
                    send_managed_pointer_leave(
                        self.input_sender.as_mut(),
                        &mut self.managed_pointer_target,
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
                let logical_key = plugin_logical_key(&event.logical_key);
                if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                    eprintln!(
                        "plugin keyboard input logical={logical_key:?} physical={:?} \
                         state={:?} synthetic={is_synthetic}",
                        event.physical_key, event.state
                    );
                }
                let policy_evaluation = event.physical_key.to_scancode().and_then(|scancode| {
                    let snapshot = renderer.key_policy_snapshot();
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
                            self.wasm_runtime.as_ref(),
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
                    if let Err(error) = sent {
                        eprintln!("dropped plugin key event: {error:#}");
                    } else if event.state == ElementState::Pressed
                        && self.pending_plugin_input.is_none()
                    {
                        self.pending_plugin_input = Some(Instant::now());
                        self.pending_plugin_scene = false;
                    }
                    if event.state == ElementState::Pressed
                        && let Some(text) = event.text.as_ref()
                        && !text.is_empty()
                    {
                        if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
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
                        brood_ipc::INPUT_KEYBOARD_KEY,
                        [
                            u64::from(scancode + 8),
                            u64::from(event.state == ElementState::Pressed),
                            0,
                            0,
                            0,
                        ],
                    );
                }
            }
            WindowEvent::RedrawRequested => match renderer.render() {
                RenderOutcome::Presented {
                    sequence,
                    reconfigure,
                } => {
                    self.renderer_recoveries = 0;
                    if self.pending_plugin_scene
                        && let Some(started) = self.pending_plugin_input.take()
                    {
                        let elapsed = started.elapsed().as_micros();
                        self.plugin_key_to_present_samples =
                            self.plugin_key_to_present_samples.saturating_add(1);
                        self.plugin_last_key_to_present_us = elapsed;
                        self.plugin_max_key_to_present_us =
                            self.plugin_max_key_to_present_us.max(elapsed);
                        self.pending_plugin_scene = false;
                    }
                    if reconfigure {
                        renderer.reconfigure();
                    }
                    if let Some(sequence) = sequence
                        && self.last_presented_sequence != Some(sequence)
                        && let Some(sender) = self.input_sender.as_mut()
                    {
                        if send_input(
                            sender,
                            brood_ipc::INPUT_FRAME_PRESENTED,
                            [sequence, 0, 0, 0, 0],
                        ) {
                            self.last_presented_sequence = Some(sequence);
                        } else {
                            renderer.window.request_redraw();
                        }
                    }
                }
                RenderOutcome::Skipped => {}
                RenderOutcome::Reconfigure => renderer.reconfigure(),
                RenderOutcome::Failed(message) => {
                    let generation = renderer.generation;
                    if let Err(error) = self.recover_renderer(generation, message) {
                        self.fail(event_loop, error);
                    }
                }
            },
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
    available_windows: Vec<AvailableWindow>,
    active_session: u64,
    current_windows: Vec<SceneWindow>,
    current_sequence: Option<u64>,
    plugin_graphics: PluginGraphicsBridge,
    plugin_panes: BTreeMap<u64, PluginPaneTexture>,
    focused_pane: Option<FocusedPane>,
    pending_focus_pane: Option<u64>,
    accepted_layout: ResolvedLayout,
    layout_source: LayoutSource,
    layout_state_dirty: bool,
    sent_managed_sizes: HashMap<u64, (u32, u32)>,
    sent_plugin_sizes: HashMap<u64, PluginPaneSize>,
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
    window_id: u64,
    resource_id: u64,
    source_rect: [i32; 4],
    rect: [i32; 4],
    active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct AvailableWindow {
    pane: u64,
    session: u64,
    app_id: String,
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

struct ImportedFrame {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
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
        brood_ipc::INPUT_PRESENTATION_SIZE,
        [
            u64::from(size.width.max(1)),
            u64::from(size.height.max(1)),
            u64::from(refresh_millihz),
            0,
            0,
        ],
    );
}

fn flush_renderer_managed_resize_requests(lane: Option<&mut InputLane>, renderer: &mut Renderer) {
    let Some(lane) = lane else {
        return;
    };
    for (window, width, height) in renderer.pending_managed_resize_requests() {
        if send_input(
            lane,
            brood_ipc::INPUT_WINDOW_RESIZE,
            [window, u64::from(width), u64::from(height), 0, 0],
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

fn send_input(lane: &mut InputLane, opcode: u32, arguments: [u64; 5]) -> bool {
    lane.flush_deferred();
    lane.sequence = lane.sequence.wrapping_add(1);
    let record = brood_ipc::CriticalRecord {
        opcode,
        connection_generation: lane.generation,
        sequence: lane.sequence,
        arguments,
        ..brood_ipc::CriticalRecord::default()
    };
    if lane.has_deferred() {
        lane.defer(record);
        return true;
    }
    if let Err(record) = lane.producer.try_send(record) {
        lane.defer(record);
        return true;
    }
    if let Err(error) = lane.notifications.notify() {
        eprintln!("failed to notify presentation input consumer: {error}");
    }
    true
}

fn send_window_focus(lane: &mut InputLane, window_id: u64) {
    let _sent = send_input(lane, brood_ipc::INPUT_WINDOW_FOCUS, [window_id, 0, 0, 0, 0]);
}

fn send_keyboard_reset(lane: &mut InputLane) {
    let _sent = send_input(lane, brood_ipc::INPUT_KEYBOARD_RESET, [0, 0, 0, 0, 0]);
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
    runtime: Option<&PresentationWasmRuntime>,
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
                } else {
                    eprintln!("key-policy close ignored for a presentation-local actor pane");
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
        HostMessage::ToggleShortcutPassthrough | HostMessage::SetShortcutPassthrough { .. } => None,
        HostMessage::SetResolvedLayout { .. }
        | HostMessage::ClaimRootLayout
        | HostMessage::DelegateLayoutRest { .. } => {
            return Err(anyhow!(
                "key policy cannot mutate layout authority directly; it must message a layout actor"
            ));
        }
    };

    if let Some(change) = focus_change {
        dispatch_focus_change(runtime, input_sender.as_mut(), change, window_focused)?;
    }
    Ok(())
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
    input_sender: Option<&mut InputLane>,
    pending_plugin_pointer_motion: &mut Option<(u64, f32, f32)>,
    managed_pointer_target: &mut Option<u64>,
) {
    let target = pointer_target_for_cursor(renderer, cursor_position, grab.as_ref());
    if let (Some(grab), Some(target)) = (grab.as_mut(), target) {
        grab.target = target;
    }
    match target {
        Some(PanePointerTarget::Plugin { pane, x, y }) => {
            *pending_plugin_pointer_motion = Some((pane, x, y));
            send_managed_pointer_leave(input_sender, managed_pointer_target);
        }
        Some(PanePointerTarget::Managed { pane, x, y }) => {
            *pending_plugin_pointer_motion = None;
            if let Some(sender) = input_sender
                && send_input(
                    sender,
                    brood_ipc::INPUT_WINDOW_POINTER_MOTION,
                    [pane, x.to_bits(), y.to_bits(), 0, 0],
                )
            {
                *managed_pointer_target = Some(pane);
            }
        }
        None => {
            *pending_plugin_pointer_motion = None;
            send_managed_pointer_leave(input_sender, managed_pointer_target);
        }
    }
}

fn send_pointer_button_to_target(
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    pending_plugin_pointer_motion: &mut Option<(u64, f32, f32)>,
    managed_pointer_target: &mut Option<u64>,
    target: PanePointerTarget,
    button: PointerButtonState,
) {
    match target {
        PanePointerTarget::Plugin { pane, x, y } => {
            *pending_plugin_pointer_motion = None;
            send_managed_pointer_leave(input_sender, managed_pointer_target);
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
            *pending_plugin_pointer_motion = None;
            if let Some(sender) = input_sender
                && send_input(
                    sender,
                    brood_ipc::INPUT_WINDOW_POINTER_BUTTON,
                    [
                        pane,
                        u64::from(button.button),
                        u64::from(button.pressed),
                        x.to_bits(),
                        y.to_bits(),
                    ],
                )
            {
                *managed_pointer_target = Some(pane);
            }
        }
    }
}

fn cancel_pointer_grab(
    runtime: Option<&PresentationWasmRuntime>,
    input_sender: Option<&mut InputLane>,
    pending_plugin_pointer_motion: &mut Option<(u64, f32, f32)>,
    managed_pointer_target: &mut Option<u64>,
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
            pending_plugin_pointer_motion,
            managed_pointer_target,
            grab.target,
            PointerButtonState {
                button,
                pressed: false,
                modifiers,
            },
        );
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
    if !send_input(lane, brood_ipc::INPUT_WINDOW_POINTER_LEAVE, [0, 0, 0, 0, 0]) {
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
    fn new(
        device: &wgpu::Device,
        bind_group_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        window_id: u64,
        resource_id: u64,
        frame: DmabufFrame,
    ) -> Result<Self> {
        if frame.color_space != brood_ipc::FrameColorSpace::Srgb {
            return Err(anyhow!(
                "unsupported happy-path DMA-BUF color space {:?}",
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("hosted DMA-BUF sRGB view"),
            format: Some(view_format),
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hosted DMA-BUF bind group"),
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
        Ok(Self {
            _texture: texture,
            bind_group,
        })
    }
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
            "configured presentation surface format={:?} color_space={:?}",
            config.format, config.color_space
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
            available_windows: Vec::new(),
            active_session: 0,
            current_windows: Vec::new(),
            current_sequence: None,
            plugin_graphics,
            plugin_panes: BTreeMap::new(),
            focused_pane: None,
            pending_focus_pane: None,
            accepted_layout: ResolvedLayout::empty(),
            layout_source: LayoutSource::NativeFallback,
            layout_state_dirty: true,
            sent_managed_sizes: HashMap::new(),
            sent_plugin_sizes: HashMap::new(),
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
        active_session: u64,
        windows: Vec<PresentationWindowSummary>,
    ) -> Option<FocusChange> {
        let previous_inventory = self
            .available_windows
            .iter()
            .map(|window| (window.pane, window.session, window.app_id.clone()))
            .collect::<Vec<_>>();
        let previous_active = self.active_session;
        let mut available_windows = windows
            .into_iter()
            .map(|window| AvailableWindow {
                pane: window.pane,
                session: window.session,
                app_id: window.app_id,
            })
            .collect::<Vec<_>>();
        available_windows.sort_by_key(|window| window.pane);
        let inventory = available_windows
            .iter()
            .map(|window| (window.pane, window.session, window.app_id.clone()))
            .collect::<Vec<_>>();
        if previous_inventory == inventory && previous_active == active_session {
            return None;
        }

        self.available_windows = available_windows;
        self.active_session = active_session;
        self.layout_state_dirty = true;
        if self.layout_source == LayoutSource::NativeFallback {
            return self.apply_default_session_layout(active_session);
        }
        self.window.request_redraw();
        self.reconcile_layout()
    }

    fn select_active_session(&mut self, active_session: u64) -> Option<FocusChange> {
        if self.active_session == active_session {
            return None;
        }
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
        sequence: u64,
        _width: u32,
        _height: u32,
        windows: Vec<SceneWindowUpdate>,
    ) -> Result<Option<FocusChange>> {
        let previous_focus = self.focused_pane;
        let previous_managed_panes = self
            .current_windows
            .iter()
            .map(|window| window.window_id)
            .collect::<Vec<_>>();
        let mut scene_windows = Vec::with_capacity(windows.len());
        let mut backend_focus = None;
        for window in windows {
            let previous_rect = self
                .current_windows
                .iter()
                .find(|current| current.window_id == window.window_id)
                .map_or(window.rect, |current| current.rect);
            if let Some(frame) = window.resource {
                let imported = ImportedFrame::new(
                    &self.device,
                    &self.frame_bind_group_layout,
                    &self.frame_sampler,
                    window.window_id,
                    window.resource_id,
                    frame,
                )?;
                self.imported_frames.insert(window.resource_id, imported);
            }
            if window.active {
                backend_focus = Some(window.window_id);
            }
            scene_windows.push(SceneWindow {
                window_id: window.window_id,
                resource_id: window.resource_id,
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
            self.current_windows = scene_windows;
            self.current_sequence = Some(sequence);
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
                self.plugin_panes.keys().copied(),
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
            return Ok(focus_change);
        }
        Ok(None)
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
        let Some(target) = self.focus_target_for_pane(pane) else {
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
            panes: self.pane_inventory(),
            managed_panes: self
                .available_windows
                .iter()
                .map(|window| window.pane)
                .collect(),
            app_panes: self
                .available_windows
                .iter()
                .filter(|window| !window.app_id.is_empty())
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
            .retain(|pane, _| self.plugin_panes.contains_key(pane));
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
        self.plugin_panes
            .keys()
            .copied()
            .chain(self.available_windows.iter().map(|window| window.pane))
            .collect()
    }

    fn default_visible_pane_inventory(&self) -> Vec<u64> {
        default_visible_panes(
            self.plugin_panes.keys().copied(),
            &self.available_windows,
            self.active_session,
        )
    }

    const fn focused_pane_id(&self) -> Option<u64> {
        match self.focused_pane {
            Some(FocusedPane::Plugin(pane) | FocusedPane::Managed(pane)) => Some(pane),
            None => None,
        }
    }

    fn focus_target_for_pane(&self, pane: u64) -> Option<FocusedPane> {
        if self.plugin_panes.contains_key(&pane) {
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
        let scale = self.window.scale_factor() as f32;
        self.plugin_panes
            .iter()
            .filter_map(|(pane, plugin)| {
                let (width, height) = positive_rect_extent(plugin.rect)?;
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

    fn resize(&mut self, width: u32, height: u32) -> Result<Option<FocusChange>> {
        if width == 0 || height == 0 {
            return Ok(None);
        }

        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
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

        let submission = self.queue.submit([encoder.finish()]);
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
        self.queue.present(frame);
        RenderOutcome::Presented {
            sequence: self.current_sequence,
            reconfigure,
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

fn default_visible_panes(
    plugin_panes: impl IntoIterator<Item = u64>,
    windows: &[AvailableWindow],
    active_session: u64,
) -> Vec<u64> {
    plugin_panes
        .into_iter()
        .chain(
            windows
                .iter()
                .filter(|window| window.session == active_session)
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
enum RenderOutcome {
    Presented {
        sequence: Option<u64>,
        reconfigure: bool,
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

    fn available(pane: u64, session: u64) -> AvailableWindow {
        AvailableWindow {
            pane,
            session,
            app_id: format!("app-{pane}"),
        }
    }

    #[test]
    fn session_selection_filters_default_view_without_reducing_layout_authority() {
        let windows = vec![available(10, 1), available(20, 2), available(30, 2)];
        assert_eq!(
            default_visible_panes([u64::MAX], &windows, 1),
            [u64::MAX, 10]
        );
        assert_eq!(
            default_visible_panes([u64::MAX], &windows, 2),
            [u64::MAX, 20, 30]
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
    fn mapping_readiness_is_not_part_of_default_layout_input() {
        let windows = vec![available(41, 7), available(42, 8)];
        assert_eq!(default_visible_panes([], &windows, 7), [41]);
        assert!(managed_pane_uses_placeholder(true, false));
        assert!(!managed_pane_uses_placeholder(true, true));
        assert!(!managed_pane_uses_placeholder(false, false));
    }
}
