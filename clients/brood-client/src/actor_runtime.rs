use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use brood_protocol::{BackendActorRealm, BackendId};
use brood_wasm::{
    ActorId, ComponentActor, ComponentActorConfig, ComponentActorEvent, ComponentActorEventWaker,
    ComponentActorPresentationEnvelopeSendError, ComponentActorSendError, ComponentChildEvent,
    FilesystemGrant, HostedWasmActor, LegacyWasmActorGeneration, LegacyWasmRuntimeEvent, Mailbox,
    MailboxAddress, NativeFilesystemWatcherService, PluginLifecycleEntry, PluginLifecycleEvent,
    PluginStartContext, PluginStartReason, PresentationEvent, PresentationScope,
    Preview1ActorCapabilities, Preview1CompatActor, Preview1FilesystemGrant, Preview1WorkerConfig,
    Registry, RuntimeIoCapabilities, RuntimeIoConfig, SessionCommandRequest, SessionEvent,
    SessionSnapshot, WasmActor, WasmActorDeclaration, WasmActorEventKind, WasmPluginArtifact,
};
use tokio::sync::{Mutex, mpsc};
use winit::event_loop::EventLoopProxy;

use crate::{ClientEvent, plugin_graphics::PluginGraphicsBridge, render_scheduler::ActorScope};

const CLIENT_INIT_ACTOR_ID: &str = "brood.client-init";
const ACTOR_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(2);
const ACTOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_START_MESSAGES: usize = 64;
const MAX_PENDING_START_MESSAGE_BYTES: usize = 1024 * 1024;
pub const ACTOR_OUTPUT_BACKLOG: usize = 256;
const ACTOR_INPUT_FLUSH_QUANTUM: usize = 64;
const ACTOR_OUTPUT_PER_ACTOR_QUANTUM: usize = 32;
const ACTOR_OUTPUT_TOTAL_QUANTUM: usize = 128;

type PendingStartMessage = (String, Vec<u8>);
type PendingStartMailboxes = BTreeMap<String, VecDeque<PendingStartMessage>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct PresentationActorSpec {
    id: String,
    path: PathBuf,
    pane_count: usize,
}

pub struct PresentationWasmRuntime {
    executor: tokio::runtime::Runtime,
    io_executor: tokio::runtime::Runtime,
    actors: BTreeMap<String, PresentationActor>,
    legacy_actors: BTreeMap<String, LegacyPresentationActor>,
    legacy_registry: Arc<Mutex<Registry>>,
    host_mailbox: Mailbox,
    filesystem_watcher_service: NativeFilesystemWatcherService,
    event_proxy: Option<EventLoopProxy<ClientEvent>>,
    graphics: Option<PluginGraphicsBridge>,
    init: Option<PresentationInitState>,
    failed_init_children: BTreeSet<String>,
    starting_init_children: BTreeSet<String>,
    stopping_init_children: BTreeSet<String>,
    stop_requested_while_starting: BTreeSet<String>,
    lifecycle_completions: mpsc::Receiver<ActorLifecycleCompletion>,
    lifecycle_completion_sender: mpsc::Sender<ActorLifecycleCompletion>,
    lifecycle_tasks: Vec<tokio::task::JoinHandle<()>>,
    pending_start_messages: StdMutex<PendingStartMailboxes>,
    outbound_backend_messages: VecDeque<BackendActorMessageRequest>,
    outbound_session_commands: VecDeque<SessionCommandEnvelope>,
    outbound_host_messages: VecDeque<HostActorMessage>,
    started_panes_awaiting_start_barrier: VecDeque<u64>,
    pending_pane_creations: VecDeque<u64>,
    pending_pane_removals: VecDeque<u64>,
    bound_backend_id: Option<BackendId>,
    bound_session_id: Option<u64>,
    connected_backends: BTreeSet<BackendId>,
    next_actor_pane: u64,
    workspace_root: PathBuf,
    session_workspaces: BTreeMap<(BackendId, u64), PathBuf>,
    session_snapshots: BTreeMap<BackendId, SessionSnapshot>,
    pending_actor_events: std::sync::Mutex<VecDeque<PendingActorEvent>>,
    last_flushed_input_actor: StdMutex<Option<String>>,
    last_drained_actor: Option<String>,
    observer_output_backlog_pending: bool,
    diagnostics: Arc<ActorRuntimeCounters>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActorRuntimeDiagnostics {
    pub reliable_transition_drops: u64,
    pub actor_failures: u64,
    pub pending_actor_events: usize,
}

#[derive(Default)]
struct ActorRuntimeCounters {
    reliable_transition_drops: AtomicU64,
    actor_failures: AtomicU64,
}

enum PendingActorEvent {
    Presentation {
        actor: String,
        event: PresentationEvent,
        watermark: Option<u64>,
    },
    Session {
        actor: String,
        event: SessionEvent,
    },
    Child {
        actor: String,
        event: ComponentChildEvent,
    },
}

impl PendingActorEvent {
    fn actor(&self) -> &str {
        match self {
            Self::Presentation { actor, .. }
            | Self::Session { actor, .. }
            | Self::Child { actor, .. } => actor,
        }
    }
}

pub struct BackendActorMessageRequest {
    pub backend: BackendId,
    pub sender: String,
    pub realm: BackendActorRealm,
    pub actor: String,
    pub body: Vec<u8>,
}

pub struct SessionCommandEnvelope {
    pub actor_id: String,
    pub request: SessionCommandRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorIdentity {
    pub name: String,
    pub generation: u64,
}

pub struct HostActorMessage {
    pub sender: Option<ActorIdentity>,
    pub payload: Vec<u8>,
}

struct PresentationInitState {
    root: PathBuf,
    children: BTreeSet<String>,
}

struct PresentationActor {
    pane: u64,
    panes: BTreeSet<u64>,
    backend: Option<BackendId>,
    session: Option<u64>,
    actor: HostedWasmActor,
}

struct LegacyPresentationActor {
    declaration: WasmActorDeclaration,
    generation: LegacyWasmActorGeneration,
    mailbox: MailboxAddress,
}

enum ActorLifecycleCompletion {
    Started {
        id: String,
        generation: u64,
        path: PathBuf,
        actor: Box<PresentationActor>,
        result: std::result::Result<(), String>,
    },
    StartFailed {
        id: String,
        panes: Vec<u64>,
        message: String,
    },
    Stopped {
        id: String,
        pane: Option<u64>,
        panes: Vec<u64>,
        disposition: StopDisposition,
        result: std::result::Result<(), String>,
    },
}

enum StopDisposition {
    Requested,
    Failed,
    StartFailed(String),
}

impl PresentationWasmRuntime {
    pub fn start(
        actor_specs: &[String],
        init_actor: Option<&Path>,
        bound_session: Option<(BackendId, u64)>,
        graphics: Option<&PluginGraphicsBridge>,
        event_proxy: Option<EventLoopProxy<ClientEvent>>,
    ) -> Result<Self> {
        let workspace_root =
            std::env::current_dir().context("resolve presentation client workspace root")?;
        let specs = actor_specs
            .iter()
            .map(|spec| PresentationActorSpec::parse(spec))
            .collect::<Result<Vec<_>>>()?;
        let available_threads = std::thread::available_parallelism().map_or(2, usize::from);
        let actor_threads = available_threads.saturating_sub(1).clamp(1, 4);
        let executor = tokio::runtime::Builder::new_multi_thread()
            .thread_name("brood-client-wasm")
            .worker_threads(actor_threads)
            .enable_all()
            .build()
            .context("start presentation WASM executor")?;
        let io_executor = tokio::runtime::Builder::new_multi_thread()
            .thread_name("brood-client-io")
            .worker_threads(1)
            .enable_all()
            .build()
            .context("start presentation service executor")?;
        let mut registry = Registry::new();
        let host_mailbox = registry
            .register_host_mailbox(64)
            .context("register presentation host actor mailbox")?;
        // Lifecycle operations are already bounded by the number of admitted
        // actors. Keep their completed values bounded too: each completion
        // owns an actor and can therefore retain a complete Wasmtime store.
        let (lifecycle_completion_sender, lifecycle_completions) = mpsc::channel(64);
        let mut runtime = Self {
            executor,
            io_executor,
            actors: BTreeMap::new(),
            legacy_actors: BTreeMap::new(),
            legacy_registry: Arc::new(Mutex::new(registry)),
            host_mailbox,
            filesystem_watcher_service: NativeFilesystemWatcherService::new(),
            event_proxy,
            graphics: graphics.cloned(),
            init: None,
            failed_init_children: BTreeSet::new(),
            starting_init_children: BTreeSet::new(),
            stopping_init_children: BTreeSet::new(),
            stop_requested_while_starting: BTreeSet::new(),
            lifecycle_completions,
            lifecycle_completion_sender,
            lifecycle_tasks: Vec::new(),
            pending_start_messages: StdMutex::new(BTreeMap::new()),
            outbound_backend_messages: VecDeque::new(),
            outbound_session_commands: VecDeque::new(),
            outbound_host_messages: VecDeque::new(),
            started_panes_awaiting_start_barrier: VecDeque::new(),
            pending_pane_creations: VecDeque::new(),
            pending_pane_removals: VecDeque::new(),
            bound_backend_id: bound_session.map(|(backend, _session)| backend),
            bound_session_id: bound_session.map(|(_backend, session)| session),
            connected_backends: bound_session
                .map(|(backend, _session)| BTreeSet::from([backend]))
                .unwrap_or_default(),
            next_actor_pane: u64::MAX,
            workspace_root,
            session_workspaces: BTreeMap::new(),
            session_snapshots: BTreeMap::new(),
            pending_actor_events: std::sync::Mutex::new(VecDeque::new()),
            last_flushed_input_actor: StdMutex::new(None),
            last_drained_actor: None,
            observer_output_backlog_pending: false,
            diagnostics: Arc::new(ActorRuntimeCounters::default()),
        };
        if let Some(path) = init_actor {
            runtime.load_init_actor(path, bound_session, graphics)?;
        }
        for spec in specs {
            let panes = runtime.allocate_actor_panes(spec.pane_count)?;
            let id = spec.id.clone();
            runtime.load_actor(spec, panes, None, None, graphics, &[])?;
            runtime.start_direct_component_actor(&id)?;
        }
        Ok(runtime)
    }

    pub fn diagnostics(&self) -> ActorRuntimeDiagnostics {
        ActorRuntimeDiagnostics {
            reliable_transition_drops: self
                .diagnostics
                .reliable_transition_drops
                .load(Ordering::Relaxed),
            actor_failures: self.diagnostics.actor_failures.load(Ordering::Relaxed),
            pending_actor_events: self
                .pending_actor_events
                .lock()
                .map_or(ACTOR_OUTPUT_BACKLOG, |pending| pending.len()),
        }
    }

    pub fn pending_outbound_host_messages(&self) -> usize {
        self.outbound_host_messages
            .len()
            .saturating_add(self.host_mailbox.pending_len())
    }

    pub fn pending_actor_input_messages(&self) -> usize {
        self.pending_actor_events
            .lock()
            .map_or(ACTOR_OUTPUT_BACKLOG, |pending| pending.len())
    }

    pub const fn observer_output_backlog_pending(&self) -> bool {
        self.observer_output_backlog_pending
    }

    #[cfg(test)]
    pub fn retain_test_host_message(&mut self, payload: Vec<u8>) {
        self.outbound_host_messages.push_back(HostActorMessage {
            sender: None,
            payload,
        });
    }

    fn note_reliable_transition_drop(&self) {
        self.diagnostics
            .reliable_transition_drops
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_actor_failure(&self) {
        self.diagnostics
            .actor_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    fn start_direct_component_actor(&self, id: &str) -> Result<()> {
        let Some(hosted) = self.actors.get(id) else {
            anyhow::ensure!(
                self.legacy_actors.contains_key(id),
                "direct presentation actor {id:?} is unavailable"
            );
            return Ok(());
        };
        if !matches!(&hosted.actor, HostedWasmActor::Component(_)) {
            return Ok(());
        }
        let context =
            direct_actor_start_context(id, hosted.actor.generation(), &self.workspace_root);
        self.executor
            .block_on(
                hosted
                    .actor
                    .lifecycle(PluginLifecycleEvent::Start(context), Duration::from_secs(2)),
            )
            .with_context(|| format!("start direct presentation actor {id:?}"))
    }

    fn allocate_actor_pane(&mut self) -> Result<u64> {
        let pane = self.next_actor_pane;
        ensure!(pane != 0, "presentation actor pane ID space is exhausted");
        self.next_actor_pane = pane.saturating_sub(1);
        Ok(pane)
    }

    fn allocate_actor_panes(&mut self, count: usize) -> Result<Vec<u64>> {
        ensure!(
            (1..=16).contains(&count),
            "presentation actor pane count must be 1..=16"
        );
        (0..count).map(|_| self.allocate_actor_pane()).collect()
    }

    /// Install one verified network artifact without granting management or
    /// runtime-I/O capabilities.
    pub fn install_artifact_actor(&mut self, id: &str, path: &Path) -> Result<u64> {
        ensure!(
            !id.trim().is_empty() && id != CLIENT_INIT_ACTOR_ID,
            "remote presentation actor id is unavailable"
        );
        let pane = self.allocate_actor_pane()?;
        self.queue_hosted_actor_start(
            PresentationActorSpec {
                id: id.to_string(),
                path: path.to_path_buf(),
                pane_count: 1,
            },
            vec![pane],
            None,
            None,
            &[],
        )?;
        Ok(pane)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the initialized root actor is transferred into the long-lived client realm"
    )]
    fn load_init_actor(
        &mut self,
        path: &Path,
        bound_session: Option<(BackendId, u64)>,
        graphics: Option<&PluginGraphicsBridge>,
    ) -> Result<()> {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve client init actor {}", path.display()))?;
        let artifact = WasmPluginArtifact::load(&path)?;
        ensure!(
            matches!(artifact, WasmPluginArtifact::Component { .. }),
            "realm init actor must be a Component Model actor"
        );
        let spec = PresentationActorSpec {
            id: CLIENT_INIT_ACTOR_ID.to_string(),
            path: path.clone(),
            pane_count: 0,
        };
        let actor = self.executor.block_on(load_hosted_actor(
            &spec,
            self.filesystem_watcher_service.clone(),
            None,
            artifact,
            true,
            &[],
            &self.workspace_root,
            self.event_proxy.clone(),
            Some(self.io_executor.handle().clone()),
        ))?;
        self.executor.block_on(actor.lifecycle(
            PluginLifecycleEvent::Start(PluginStartContext {
                instance_id: CLIENT_INIT_ACTOR_ID.to_string(),
                generation: actor.generation(),
                reason: PluginStartReason::Initial,
                backend_id: bound_session.map(|(backend, _session)| backend.0),
                session_id: bound_session.map(|(_backend, session)| session),
                workspace_root: ".".to_string(),
                data: vec![PluginLifecycleEntry {
                    key: "realm".to_string(),
                    value: "client".to_string(),
                }],
            }),
            Duration::from_secs(2),
        ))?;
        self.actors.insert(
            CLIENT_INIT_ACTOR_ID.to_string(),
            PresentationActor {
                pane: 0,
                panes: BTreeSet::new(),
                backend: None,
                session: None,
                actor,
            },
        );
        self.init = Some(PresentationInitState {
            root: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            children: BTreeSet::new(),
        });
        self.drain_events_with_graphics(graphics)
    }

    #[allow(
        clippy::significant_drop_tightening,
        clippy::too_many_arguments,
        reason = "the loaded actor is intentionally transferred into the long-lived client runtime"
    )]
    fn load_actor(
        &mut self,
        spec: PresentationActorSpec,
        panes: Vec<u64>,
        backend: Option<BackendId>,
        session: Option<u64>,
        graphics: Option<&PluginGraphicsBridge>,
        capabilities: &[String],
    ) -> Result<()> {
        ensure!(
            !self.actors.contains_key(&spec.id) && !self.legacy_actors.contains_key(&spec.id),
            "duplicate presentation actor id {:?}",
            spec.id
        );
        match WasmPluginArtifact::load(&spec.path)? {
            WasmPluginArtifact::Legacy(artifact) => {
                self.load_legacy_actor(spec, artifact.inventory.actors)
            }
            artifact => self.load_hosted_actor(
                spec,
                panes,
                backend,
                session,
                graphics,
                artifact,
                capabilities,
            ),
        }
    }

    fn load_legacy_actor(
        &mut self,
        spec: PresentationActorSpec,
        declarations: Vec<WasmActorDeclaration>,
    ) -> Result<()> {
        let declaration = declarations
            .into_iter()
            .find(|declaration| declaration.name == spec.id)
            .with_context(|| {
                format!(
                    "{} does not declare actor {:?}",
                    spec.path.display(),
                    spec.id
                )
            })?;
        let registry = Arc::clone(&self.legacy_registry);
        let (registration, mailbox) = self.executor.block_on({
            let registry = Arc::clone(&registry);
            let actor_id = spec.id.clone();
            async move { registry.lock().await.prepare_actor(actor_id, 64) }
        })?;
        let actor = self
            .executor
            .block_on(WasmActor::load_named(
                &spec.path,
                mailbox,
                &declaration.export_name,
            ))
            .with_context(|| format!("load presentation actor {:?}", spec.id))?;
        let actor_id = actor.actor_id();
        let mailbox_address = self.executor.block_on({
            let registry = Arc::clone(&registry);
            async move {
                let mut registry = registry.lock().await;
                registry.activate_prepared_actor_with_address(registration, &[])
            }
        })?;

        let proxy = self.event_proxy.clone();
        let mut actor_ids = BTreeMap::new();
        actor_ids.insert(spec.id.clone(), actor_id);
        let generation = LegacyWasmActorGeneration::start(
            self.executor.handle(),
            vec![(declaration.clone(), actor)],
            actor_ids,
            registry,
            &spec.path,
            move |event| {
                let LegacyWasmRuntimeEvent::DrainHostMailbox = event;
                if let Some(proxy) = &proxy {
                    drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                }
            },
        );
        eprintln!(
            "started presentation WASM actor id={} kind=core path={}",
            spec.id,
            spec.path.display()
        );
        self.legacy_actors.insert(
            spec.id,
            LegacyPresentationActor {
                declaration,
                generation,
                mailbox: mailbox_address,
            },
        );
        Ok(())
    }

    #[allow(
        clippy::significant_drop_tightening,
        clippy::too_many_arguments,
        reason = "the loaded actor is intentionally transferred into the long-lived client runtime"
    )]
    fn load_hosted_actor(
        &mut self,
        spec: PresentationActorSpec,
        panes: Vec<u64>,
        backend: Option<BackendId>,
        session: Option<u64>,
        graphics: Option<&PluginGraphicsBridge>,
        artifact: WasmPluginArtifact,
        capabilities: &[String],
    ) -> Result<()> {
        let watcher = self.filesystem_watcher_service.clone();
        let pane = panes
            .first()
            .copied()
            .context("actor has no primary pane")?;
        let presentation =
            graphics.map(|graphics| graphics.actor_host(spec.id.clone(), panes.iter().copied()));
        let workspace_root = backend
            .zip(session)
            .and_then(|realm| self.session_workspaces.get(&realm))
            .unwrap_or(&self.workspace_root)
            .clone();
        let actor = self.executor.block_on(load_hosted_actor(
            &spec,
            watcher,
            presentation.clone(),
            artifact,
            false,
            capabilities,
            &workspace_root,
            self.event_proxy.clone(),
            Some(self.io_executor.handle().clone()),
        ))?;
        if let Some(presentation) = presentation {
            presentation.bind_generation(actor.generation());
        }
        eprintln!(
            "{}",
            started_hosted_actor_log(&spec.id, actor.kind(), actor.generation(), pane, &spec.path,)
        );
        self.actors.insert(
            spec.id,
            PresentationActor {
                pane,
                panes: panes.into_iter().collect(),
                backend,
                session,
                actor,
            },
        );
        Ok(())
    }

    /// Schedule dynamic actor loading and its start lifecycle entirely on the
    /// actor executor. This method is called by Winit-facing install and init
    /// paths, so it must never wait for Wasmtime or Tokio work inline.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the loaded actor and completion payload deliberately move between the async loader and bounded completion lane"
    )]
    fn queue_hosted_actor_start(
        &mut self,
        spec: PresentationActorSpec,
        panes: Vec<u64>,
        backend: Option<BackendId>,
        session: Option<u64>,
        capabilities: &[String],
    ) -> Result<()> {
        ensure!(
            !self.actors.contains_key(&spec.id)
                && !self.legacy_actors.contains_key(&spec.id)
                && !self.starting_init_children.contains(&spec.id)
                && !self.stopping_init_children.contains(&spec.id),
            "duplicate presentation actor id {:?}",
            spec.id
        );
        let artifact = WasmPluginArtifact::load(&spec.path)?;
        ensure!(
            !matches!(artifact, WasmPluginArtifact::Legacy(_)),
            "dynamic presentation actors must use the hosted Component or Preview 1 runtime"
        );
        let pane = panes
            .first()
            .copied()
            .context("actor has no primary pane")?;
        let presentation = self
            .graphics
            .as_ref()
            .map(|graphics| graphics.actor_host(spec.id.clone(), panes.iter().copied()));
        let workspace_root = backend
            .zip(session)
            .and_then(|realm| self.session_workspaces.get(&realm))
            .unwrap_or(&self.workspace_root)
            .clone();
        let watcher = self.filesystem_watcher_service.clone();
        let proxy = self.event_proxy.clone();
        let completion = self.lifecycle_completion_sender.clone();
        let diagnostics = Arc::clone(&self.diagnostics);
        let io_runtime = self.io_executor.handle().clone();
        let capabilities = capabilities.to_vec();
        let id = spec.id.clone();
        let completion_panes = panes.clone();
        self.starting_init_children.insert(id.clone());
        self.failed_init_children.remove(&id);
        let task = self.executor.spawn(async move {
            let loaded = load_hosted_actor(
                &spec,
                watcher,
                presentation.clone(),
                artifact,
                false,
                &capabilities,
                &workspace_root,
                proxy.clone(),
                Some(io_runtime),
            )
            .await;
            let completion_event = match loaded {
                Ok(actor) => {
                    let generation = actor.generation();
                    if let Some(presentation) = presentation {
                        presentation.bind_generation(generation);
                    }
                    let lifecycle = PluginLifecycleEvent::Start(PluginStartContext {
                        instance_id: id.clone(),
                        generation,
                        reason: PluginStartReason::Initial,
                        backend_id: backend.map(|backend| backend.0),
                        session_id: session,
                        workspace_root: workspace_root.to_string_lossy().into_owned(),
                        data: vec![PluginLifecycleEntry {
                            key: "realm".to_string(),
                            value: "client".to_string(),
                        }],
                    });
                    let result = run_bounded_actor_operation(
                        "dynamic actor start lifecycle",
                        ACTOR_LIFECYCLE_TIMEOUT,
                        actor.lifecycle(lifecycle, ACTOR_LIFECYCLE_TIMEOUT),
                    )
                    .await
                    .map_err(|error| format!("dynamic actor start lifecycle failed: {error}"));
                    ActorLifecycleCompletion::Started {
                        id,
                        generation,
                        path: spec.path.clone(),
                        actor: Box::new(PresentationActor {
                            pane,
                            panes: panes.into_iter().collect(),
                            backend,
                            session,
                            actor,
                        }),
                        result,
                    }
                }
                Err(error) => ActorLifecycleCompletion::StartFailed {
                    id,
                    panes: completion_panes,
                    message: format!("load dynamic presentation actor: {error:#}"),
                },
            };
            // Wake before awaiting capacity so the Winit owner drains an
            // already-full completion lane instead of waiting for a send that
            // cannot finish.
            wake_lifecycle_completion(proxy.as_ref());
            if completion.send(completion_event).await.is_err() {
                diagnostics
                    .reliable_transition_drops
                    .fetch_add(1, Ordering::Relaxed);
            }
            wake_lifecycle_completion(proxy.as_ref());
        });
        self.lifecycle_tasks.push(task);
        Ok(())
    }

    pub fn drain_events(&mut self) -> Result<()> {
        self.flush_pending_actor_events()?;
        // A retained input means the component command lane is full. The actor
        // may be waiting for us to consume its observer output before it can
        // service that lane, so output must keep flowing while input retries.
        let graphics = self.graphics.clone();
        self.drain_events_with_graphics(graphics.as_ref())
    }

    fn flush_pending_actor_events(&self) -> Result<()> {
        let mut pending = self.pending_actor_events.lock().map_err(|error| {
            self.note_reliable_transition_drop();
            anyhow::anyhow!("pending component actor input is poisoned: {error}")
        })?;
        let mut last_actor = self
            .last_flushed_input_actor
            .lock()
            .map_err(|error| {
                self.note_reliable_transition_drop();
                anyhow::anyhow!("pending component actor input cursor is poisoned: {error}")
            })?
            .clone();
        let mut blocked_actors = BTreeSet::new();
        let mut processed = 0;
        while processed < ACTOR_INPUT_FLUSH_QUANTUM {
            let Some(index) =
                next_flushable_actor_event(&pending, &blocked_actors, last_actor.as_deref())
            else {
                break;
            };
            let Some(command) = pending.remove(index) else {
                drop(pending);
                self.note_reliable_transition_drop();
                anyhow::bail!("selected pending actor event disappeared during flush");
            };
            let command_actor = command.actor().to_owned();
            last_actor = Some(command_actor.clone());
            processed += 1;
            let retry = match command {
                PendingActorEvent::Presentation {
                    actor,
                    event,
                    watermark,
                } => {
                    let mut envelope = vec![(event, watermark)];
                    if envelope
                        .first()
                        .is_some_and(|(event, _)| !pending_presentation_is_boundary(event))
                    {
                        while envelope.len() < 16 {
                            let Some(next_index) = pending.iter().position(|pending| {
                                matches!(pending, PendingActorEvent::Presentation { actor: pending_actor, .. } if pending_actor == &actor)
                            }) else {
                                break;
                            };
                            let Some(PendingActorEvent::Presentation { event, .. }) =
                                pending.get(next_index)
                            else {
                                break;
                            };
                            if pending_presentation_is_boundary(event) {
                                break;
                            }
                            let Some(PendingActorEvent::Presentation {
                                event, watermark, ..
                            }) = pending.remove(next_index)
                            else {
                                break;
                            };
                            envelope.push((event, watermark));
                        }
                    }
                    match self
                        .component_actor(&actor)?
                        .try_send_presentation_events(envelope)
                    {
                        Ok(()) => None,
                        Err(ComponentActorPresentationEnvelopeSendError::Full(events)) => {
                            for (offset, (event, watermark)) in events.into_iter().enumerate() {
                                pending.insert(
                                    index + offset,
                                    PendingActorEvent::Presentation {
                                        actor: actor.clone(),
                                        event,
                                        watermark,
                                    },
                                );
                            }
                            blocked_actors.insert(actor.clone());
                            None
                        }
                        Err(ComponentActorPresentationEnvelopeSendError::Closed(_)) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!("component actor {actor:?} command queue is closed")
                        }
                        Err(ComponentActorPresentationEnvelopeSendError::InvalidSize(_)) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!(
                                "component actor {actor:?} presentation envelope exceeded 16 events"
                            )
                        }
                        Err(ComponentActorPresentationEnvelopeSendError::InvariantViolation) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!(
                                "component actor {actor:?} returned a non-presentation command"
                            )
                        }
                    }
                }
                PendingActorEvent::Session { actor, event } => {
                    match self.component_actor(&actor)?.try_send_session_event(event) {
                        Ok(()) => None,
                        Err(ComponentActorSendError::Full(event)) => {
                            Some(PendingActorEvent::Session { actor, event })
                        }
                        Err(ComponentActorSendError::Closed(_)) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!("component actor {actor:?} command queue is closed")
                        }
                        Err(ComponentActorSendError::InvariantViolation) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!(
                                "component actor {actor:?} command queue invariant failed"
                            )
                        }
                    }
                }
                PendingActorEvent::Child { actor, event } => {
                    match self.component_actor(&actor)?.try_send_child_event(event) {
                        Ok(()) => None,
                        Err(ComponentActorSendError::Full(event)) => {
                            Some(PendingActorEvent::Child { actor, event })
                        }
                        Err(ComponentActorSendError::Closed(_)) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!("component actor {actor:?} command queue is closed")
                        }
                        Err(ComponentActorSendError::InvariantViolation) => {
                            self.note_reliable_transition_drop();
                            anyhow::bail!(
                                "component actor {actor:?} command queue invariant failed"
                            )
                        }
                    }
                }
            };
            if let Some(command) = retry {
                // Preserve this actor's exact FIFO position, but allow a
                // different actor later in the bounded queue to use its own
                // available component capacity during the same flush.
                pending.insert(index, command);
                blocked_actors.insert(command_actor);
            }
        }
        *self.last_flushed_input_actor.lock().map_err(|error| {
            self.note_reliable_transition_drop();
            anyhow::anyhow!("pending component actor input cursor is poisoned: {error}")
        })? = last_actor;
        let backlog_remains = !pending.is_empty();
        drop(pending);
        if backlog_remains && let Some(proxy) = &self.event_proxy {
            drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
        }
        Ok(())
    }

    fn component_actor(&self, id: &str) -> Result<&ComponentActor> {
        match &self
            .actors
            .get(id)
            .with_context(|| format!("presentation actor {id:?} is unavailable"))?
            .actor
        {
            HostedWasmActor::Component(actor) => Ok(actor),
            HostedWasmActor::Preview1(_) => {
                anyhow::bail!("Preview 1 actor has no component command lane")
            }
        }
    }

    fn retain_actor_event(&self, command: PendingActorEvent) -> Result<()> {
        let mut pending = match self.pending_actor_events.lock() {
            Ok(pending) => pending,
            Err(error) => {
                self.note_reliable_transition_drop();
                anyhow::bail!("pending component actor input is poisoned: {error}");
            }
        };
        if pending.len() == ACTOR_OUTPUT_BACKLOG {
            self.note_reliable_transition_drop();
            anyhow::bail!("pending component actor input backlog is full");
        }
        pending.push_back(command);
        drop(pending);
        Ok(())
    }

    fn send_ordered_presentation_event(
        &self,
        actor: &str,
        component: &ComponentActor,
        mut event: PresentationEvent,
        watermark: Option<u64>,
    ) -> Result<()> {
        let mut pending = self.pending_actor_events.lock().map_err(|error| {
            self.note_reliable_transition_drop();
            anyhow::anyhow!("pending component actor input is poisoned: {error}")
        })?;
        // Keep the latency-critical empty-FIFO path inside this ordering lock.
        // Retained ordinary events may be combined into bounded envelopes once
        // backpressure exists, but an otherwise-ready render hint must not pay
        // for a second Winit callback merely to enter the actor command lane.
        if pending.is_empty() {
            match component.try_send_presentation_event_with_watermark(event, watermark) {
                Ok(()) => return Ok(()),
                Err(ComponentActorSendError::Full(returned)) => event = returned,
                Err(ComponentActorSendError::Closed(_)) => {
                    drop(pending);
                    self.note_reliable_transition_drop();
                    anyhow::bail!("component actor command queue is closed");
                }
                Err(ComponentActorSendError::InvariantViolation) => {
                    drop(pending);
                    self.note_reliable_transition_drop();
                    anyhow::bail!("component actor command queue invariant failed");
                }
            }
        }
        let command = PendingActorEvent::Presentation {
            actor: actor.to_owned(),
            event,
            watermark,
        };
        let command = match retain_after_actor_presentation_backlog(&mut pending, command) {
            Ok(Some(command)) => command,
            Ok(None) => {
                drop(pending);
                if let Some(proxy) = &self.event_proxy {
                    drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                }
                return Ok(());
            }
            Err(_command) => {
                drop(pending);
                self.note_reliable_transition_drop();
                anyhow::bail!("pending component actor input backlog is full");
            }
        };
        if pending.len() >= ACTOR_OUTPUT_BACKLOG {
            drop(pending);
            self.note_reliable_transition_drop();
            anyhow::bail!("pending component actor input backlog is full");
        }
        pending.push_back(command);
        drop(pending);
        if let Some(proxy) = &self.event_proxy {
            drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
        }
        Ok(())
    }

    fn drain_events_with_graphics(
        &mut self,
        graphics: Option<&PluginGraphicsBridge>,
    ) -> Result<()> {
        self.drain_lifecycle_completions();
        let retained_input = self
            .pending_actor_events
            .lock()
            .map_err(|error| anyhow::anyhow!("pending component actor input is poisoned: {error}"))?
            .len();
        let mut events = Vec::new();
        let mut quantum_exhausted = false;
        self.observer_output_backlog_pending = false;
        let actor_order =
            rotated_actor_order(self.actors.keys(), self.last_drained_actor.as_deref());
        let mut last_drained_actor = None;
        for actor_id in actor_order {
            let Some(hosted) = self.actors.get_mut(&actor_id) else {
                continue;
            };
            match &mut hosted.actor {
                HostedWasmActor::Component(actor) => {
                    let mut actor_events = 0;
                    while actor_events < ACTOR_OUTPUT_PER_ACTOR_QUANTUM
                        && events.len() < ACTOR_OUTPUT_TOTAL_QUANTUM
                        && actor_output_has_capacity(
                            retained_input,
                            events.len(),
                            self.outbound_backend_messages.len(),
                            self.outbound_session_commands.len(),
                            self.outbound_host_messages.len(),
                        )
                    {
                        let Some(event) = actor.try_recv_event() else {
                            break;
                        };
                        actor_events += 1;
                        events.push((actor_id.clone(), Ok(event)));
                    }
                    quantum_exhausted |= actor_events == ACTOR_OUTPUT_PER_ACTOR_QUANTUM
                        || events.len() == ACTOR_OUTPUT_TOTAL_QUANTUM;
                    self.observer_output_backlog_pending |= observer_drain_has_more_work(
                        actor_events,
                        events.len(),
                        actor.pending_event_count(),
                    );
                    if actor_events > 0 {
                        last_drained_actor = Some(actor_id.clone());
                    }
                }
                HostedWasmActor::Preview1(actor) => {
                    if let Some(message) = actor.try_take_failure() {
                        events.push((actor_id.clone(), Err(message)));
                    }
                }
            }
            if events.len() == ACTOR_OUTPUT_TOTAL_QUANTUM {
                break;
            }
        }
        if let Some(actor) = last_drained_actor {
            self.last_drained_actor = Some(actor);
        }
        if quantum_exhausted && let Some(proxy) = &self.event_proxy {
            drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
        }
        for (actor_id, event) in events {
            let event = match event {
                Ok(event) => event,
                Err(message) if self.is_init_child(&actor_id) => {
                    self.note_actor_failure();
                    eprintln!(
                        "presentation init child actor failed id={actor_id} message={message}"
                    );
                    self.send_init_child_event(ComponentChildEvent::Failed {
                        id: actor_id.clone(),
                        message,
                    });
                    self.remove_failed_init_child(&actor_id);
                    continue;
                }
                Err(message) => {
                    self.note_actor_failure();
                    anyhow::bail!("presentation actor {actor_id:?} dispatcher failed: {message}")
                }
            };
            match event {
                ComponentActorEvent::ActorStartRequested {
                    id,
                    path,
                    capabilities,
                    presentation_scope,
                } if actor_id == CLIENT_INIT_ACTOR_ID => {
                    self.handle_init_start(id, &path, &capabilities, presentation_scope, graphics);
                }
                ComponentActorEvent::ActorStopRequested { id }
                    if actor_id == CLIENT_INIT_ACTOR_ID =>
                {
                    self.handle_init_stop(id);
                }
                ComponentActorEvent::DispatcherFailed { message }
                    if self.is_init_child(&actor_id) =>
                {
                    self.note_actor_failure();
                    eprintln!(
                        "presentation init child actor failed id={actor_id} message={message}"
                    );
                    self.send_init_child_event(ComponentChildEvent::Failed {
                        id: actor_id.clone(),
                        message,
                    });
                    self.remove_failed_init_child(&actor_id);
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    self.note_actor_failure();
                    anyhow::bail!("presentation actor {actor_id:?} dispatcher failed: {message}")
                }
                ComponentActorEvent::WorkerFailed { worker, message } => {
                    // The component runtime also delivers this typed failure to
                    // the guest actor. Workers are fallible task boundaries;
                    // the actor decides whether to render an error, retry, or
                    // otherwise recover. Treating the observer copy as fatal
                    // races that recovery and tears down the whole client.
                    eprintln!(
                        "presentation actor worker failed actor={actor_id} worker={} message={message}",
                        worker.raw()
                    );
                }
                ComponentActorEvent::WorkerStarted { worker } => {
                    if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some()
                        && !crate::PLUGIN_SCROLL_BENCH_ACTIVE
                            .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        eprintln!(
                            "presentation actor worker started actor={actor_id} worker={}",
                            worker.raw()
                        );
                    }
                }
                ComponentActorEvent::WorkerFinished { worker, output } => {
                    if std::env::var_os("BROOD_TRACE_PLUGIN_WORKERS").is_some()
                        && !crate::PLUGIN_SCROLL_BENCH_ACTIVE
                            .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        eprintln!(
                            "presentation actor worker finished actor={actor_id} worker={} bytes={}",
                            worker.raw(),
                            output.len()
                        );
                        if output.first() == Some(&b'{')
                            && let Ok(metrics) = std::str::from_utf8(&output)
                        {
                            eprintln!(
                                "presentation plugin worker metrics actor={actor_id} {metrics}"
                            );
                        }
                    }
                }
                ComponentActorEvent::WorkerCancelled { worker } => {
                    eprintln!(
                        "presentation actor worker cancelled actor={actor_id} worker={}",
                        worker.raw()
                    );
                }
                ComponentActorEvent::PresentationTurnReturned {
                    event_watermark,
                    render_token,
                    presentation_events,
                } => {
                    crate::record_plugin_scroll_presentation_turn(&actor_id, presentation_events);
                    if let (Some(graphics), Some(hosted)) = (graphics, self.actors.get(&actor_id)) {
                        graphics.complete_actor_turn(
                            &ActorScope::new(actor_id, hosted.actor.generation()),
                            event_watermark,
                            render_token.map(|token| token.0),
                        );
                    }
                }
                ComponentActorEvent::Stopped if self.is_init_child(&actor_id) => {
                    self.send_init_child_event(ComponentChildEvent::Stopped { id: actor_id });
                }
                ComponentActorEvent::Stopped => {
                    anyhow::bail!("presentation actor {actor_id:?} stopped");
                }
                ComponentActorEvent::SessionMessageRequested {
                    backend_id,
                    session_id,
                    actor,
                    body,
                } => {
                    let sender_realm = {
                        let sender = self.actors.get(&actor_id).with_context(|| {
                            format!("presentation actor {actor_id:?} is unavailable")
                        })?;
                        sender.backend.zip(sender.session)
                    };
                    let destination = (BackendId(backend_id), session_id);
                    ensure!(
                        sender_realm == Some(destination)
                            || sender_realm.is_none()
                                && (self.session_workspaces.contains_key(&destination)
                                    || self.bound_backend_id == Some(destination.0)
                                        && self.bound_session_id == Some(destination.1)),
                        "presentation actor {actor_id:?} addressed backend {} session {} outside its association",
                        destination.0.0,
                        destination.1,
                    );
                    self.outbound_backend_messages
                        .push_back(BackendActorMessageRequest {
                            backend: destination.0,
                            sender: actor_id,
                            realm: BackendActorRealm::Session(session_id),
                            actor,
                            body,
                        });
                }
                ComponentActorEvent::DaemonMessageRequested {
                    backend_id,
                    actor,
                    body,
                } => {
                    let backend = BackendId(backend_id);
                    let sender_backend = self
                        .actors
                        .get(&actor_id)
                        .with_context(|| format!("presentation actor {actor_id:?} is unavailable"))?
                        .backend;
                    ensure!(
                        sender_backend.is_none() || sender_backend == Some(backend),
                        "presentation actor {actor_id:?} addressed another backend's daemon"
                    );
                    ensure!(
                        self.connected_backends.contains(&backend),
                        "presentation actor {actor_id:?} addressed unknown backend {}",
                        backend.0,
                    );
                    self.outbound_backend_messages
                        .push_back(BackendActorMessageRequest {
                            backend,
                            sender: actor_id,
                            realm: BackendActorRealm::Daemon,
                            actor,
                            body,
                        });
                }
                ComponentActorEvent::LocalMessageRequested { actor, body } => {
                    if std::env::var_os("BROOD_TRACE_PLUGIN_INPUT").is_some() {
                        eprintln!(
                            "presentation local actor message sender={} recipient={} body={}",
                            actor_id,
                            actor,
                            String::from_utf8_lossy(&body)
                        );
                    }
                    if let Err(error) = self.send_actor_message(&actor, actor_id, body) {
                        if self.is_init_child(&actor) {
                            eprintln!(
                                "dropped local message to failed presentation child \
                                 actor={actor:?}: {error:#}"
                            );
                            continue;
                        }
                        return Err(error);
                    }
                    // The recipient can enqueue its reply after this drain pass
                    // has already sampled every actor. Wake the client loop so
                    // actor-to-actor message chains continue without requiring
                    // unrelated input or rendering activity.
                    if let Some(proxy) = &self.event_proxy {
                        drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                    }
                }
                ComponentActorEvent::SessionCommandRequested(request) => {
                    let backend = BackendId(request.backend_id);
                    ensure!(
                        self.connected_backends.contains(&backend),
                        "presentation actor {actor_id:?} requested unknown backend {}",
                        backend.0,
                    );
                    let (sender_backend, sender_session) = {
                        let sender = self.actors.get(&actor_id).with_context(|| {
                            format!("presentation actor {actor_id:?} is unavailable")
                        })?;
                        (sender.backend, sender.session)
                    };
                    if let Some(sender_backend) = sender_backend {
                        ensure!(
                            sender_backend == backend,
                            "presentation actor {actor_id:?} requested a different backend"
                        );
                    }
                    if let (Some(sender_session), Some(requested_session)) =
                        (sender_session, request.session)
                    {
                        ensure!(
                            sender_session == requested_session,
                            "presentation actor {actor_id:?} requested a different session"
                        );
                    }
                    eprintln!(
                        "queued presentation actor session command actor={} request={} operation={:?}",
                        actor_id, request.request, request.operation
                    );
                    self.outbound_session_commands
                        .push_back(SessionCommandEnvelope { actor_id, request });
                }
                ComponentActorEvent::HostMessageRequested { payload } => {
                    let sender = self
                        .actor_identity(&actor_id)
                        .context("component host message lost its actor identity")?;
                    self.outbound_host_messages.push_back(HostActorMessage {
                        sender: Some(sender),
                        payload,
                    });
                    if let Some(proxy) = &self.event_proxy {
                        drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                    }
                }
                _ => {}
            }
        }
        if self.starting_init_children.is_empty() {
            self.pending_pane_creations
                .append(&mut self.started_panes_awaiting_start_barrier);
        }
        Ok(())
    }

    fn is_init_child(&self, id: &str) -> bool {
        self.init
            .as_ref()
            .is_some_and(|init| init.children.contains(id))
            || self.failed_init_children.contains(id)
            || self.starting_init_children.contains(id)
            || self.stopping_init_children.contains(id)
    }

    fn handle_init_start(
        &mut self,
        id: String,
        guest_path: &str,
        capabilities: &[String],
        presentation_scope: PresentationScope,
        graphics: Option<&PluginGraphicsBridge>,
    ) {
        match self.start_init_child(&id, guest_path, capabilities, presentation_scope, graphics) {
            Ok(()) => {}
            Err(error) => self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: format!("{error:#}"),
            }),
        }
    }

    fn start_init_child(
        &mut self,
        id: &str,
        guest_path: &str,
        capabilities: &[String],
        presentation_scope: PresentationScope,
        graphics: Option<&PluginGraphicsBridge>,
    ) -> Result<()> {
        ensure!(
            !id.trim().is_empty(),
            "init child actor id must not be empty"
        );
        ensure!(
            id != CLIENT_INIT_ACTOR_ID,
            "init actor cannot replace itself"
        );
        ensure!(
            !self.actors.contains_key(id)
                && !self.starting_init_children.contains(id)
                && !self.stopping_init_children.contains(id),
            "init actor cannot replace an actor it does not own"
        );
        let relative = Path::new(guest_path);
        ensure!(
            !guest_path.trim().is_empty()
                && !relative.is_absolute()
                && !relative.components().any(|component| matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )),
            "init child actor path must stay inside the init package root"
        );
        let root = self
            .init
            .as_ref()
            .context("client init actor is not running")?
            .root
            .clone();
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("resolve init package root {}", root.display()))?;
        let path = root.join(relative).canonicalize().with_context(|| {
            format!("resolve init child actor {}", root.join(relative).display())
        })?;
        ensure!(
            path.starts_with(&canonical_root),
            "init child actor path must stay inside the init package root"
        );
        let spec = PresentationActorSpec {
            id: id.to_string(),
            path,
            pane_count: 1,
        };
        let pane = self.allocate_actor_pane()?;
        let realm = presentation_scope
            .session()
            .map(|(backend, session)| (BackendId(backend), session));
        if let Some(realm) = realm {
            ensure!(
                self.session_workspaces.contains_key(&realm)
                    || self.bound_backend_id == Some(realm.0)
                        && self.bound_session_id == Some(realm.1),
                "presentation child requested unknown backend {} session {}",
                realm.0.0,
                realm.1,
            );
        }
        let (backend, session) = realm.unzip();
        if let Some(init) = self.init.as_mut() {
            init.children.insert(id.to_string());
        }
        let result =
            self.queue_hosted_actor_start(spec, vec![pane], backend, session, capabilities);
        if result.is_err()
            && let Some(init) = self.init.as_mut()
        {
            init.children.remove(id);
        }
        let _ = graphics;
        result
    }

    fn handle_init_stop(&mut self, id: String) {
        if !self.is_init_child(&id) {
            self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: "init actor may stop only its own children".to_string(),
            });
            return;
        }
        if self.starting_init_children.contains(&id) {
            self.stop_requested_while_starting.insert(id);
            return;
        }
        let Some(actor) = self.actors.remove(&id) else {
            self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: "init child actor is unavailable".to_string(),
            });
            return;
        };
        if let Some(init) = self.init.as_mut() {
            init.children.remove(&id);
        }
        self.failed_init_children.remove(&id);
        self.spawn_actor_stop(id, actor, StopDisposition::Requested);
    }

    fn remove_failed_init_child(&mut self, id: &str) {
        let Some(actor) = self.actors.remove(id) else {
            return;
        };
        if let Some(init) = self.init.as_mut() {
            init.children.remove(id);
        }
        self.failed_init_children.insert(id.to_string());
        self.spawn_actor_stop(id.to_string(), actor, StopDisposition::Failed);
    }

    fn spawn_actor_stop(
        &mut self,
        id: String,
        mut actor: PresentationActor,
        disposition: StopDisposition,
    ) {
        let pane = (actor.pane != 0).then_some(actor.pane);
        let panes = actor.panes.iter().copied().collect::<Vec<_>>();
        let generation = actor.actor.generation();
        let send_stop_lifecycle = matches!(disposition, StopDisposition::Requested);
        self.starting_init_children.remove(&id);
        self.stopping_init_children.insert(id.clone());
        let completion = self.lifecycle_completion_sender.clone();
        let diagnostics = Arc::clone(&self.diagnostics);
        let proxy = self.event_proxy.clone();
        let task = self.executor.spawn(async move {
            let lifecycle_result = if send_stop_lifecycle {
                let lifecycle = PluginLifecycleEvent::Stop(brood_wasm::PluginStopContext {
                    instance_id: id.clone(),
                    generation,
                    reason: brood_wasm::PluginStopReason::Shutdown,
                    next_generation: None,
                });
                run_bounded_actor_operation(
                    "stop lifecycle",
                    ACTOR_LIFECYCLE_TIMEOUT,
                    actor.actor.lifecycle(lifecycle, ACTOR_LIFECYCLE_TIMEOUT),
                )
                .await
            } else {
                Ok(())
            };
            // Dropping the actor after a timeout invokes the runtime's hard-stop
            // fallback and aborts every owned async task.
            let shutdown_result = run_bounded_actor_operation(
                "actor shutdown",
                ACTOR_SHUTDOWN_TIMEOUT,
                actor.actor.shutdown(),
            )
            .await;
            let result = lifecycle_result.and(shutdown_result);
            wake_lifecycle_completion(proxy.as_ref());
            if completion
                .send(ActorLifecycleCompletion::Stopped {
                    id,
                    pane,
                    panes,
                    disposition,
                    result,
                })
                .await
                .is_err()
            {
                diagnostics
                    .reliable_transition_drops
                    .fetch_add(1, Ordering::Relaxed);
            }
            wake_lifecycle_completion(proxy.as_ref());
        });
        self.lifecycle_tasks.push(task);
    }

    fn drain_lifecycle_completions(&mut self) {
        self.lifecycle_tasks.retain(|task| !task.is_finished());
        for _ in 0..ACTOR_OUTPUT_TOTAL_QUANTUM {
            let Ok(completion) = self.lifecycle_completions.try_recv() else {
                break;
            };
            match completion {
                ActorLifecycleCompletion::Started {
                    id,
                    generation,
                    path,
                    actor,
                    result,
                } => {
                    self.starting_init_children.remove(&id);
                    if let Err(message) = result {
                        self.note_actor_failure();
                        self.discard_pending_start_messages(&id);
                        self.stop_requested_while_starting.remove(&id);
                        if let Some(init) = self.init.as_mut() {
                            init.children.remove(&id);
                        }
                        self.failed_init_children.insert(id.clone());
                        self.spawn_actor_stop(id, *actor, StopDisposition::StartFailed(message));
                        continue;
                    }

                    let message_result =
                        self.take_pending_start_messages(&id)
                            .and_then(|queued_messages| {
                                queued_messages
                                    .into_iter()
                                    .try_for_each(|(sender, payload)| {
                                        actor.actor.try_send_message(sender, payload)
                                    })
                            });
                    if let Err(error) = message_result {
                        self.note_actor_failure();
                        self.stop_requested_while_starting.remove(&id);
                        if let Some(init) = self.init.as_mut() {
                            init.children.remove(&id);
                        }
                        self.failed_init_children.insert(id.clone());
                        self.spawn_actor_stop(
                            id,
                            *actor,
                            StopDisposition::StartFailed(format!(
                                "deliver messages queued during start: {error:#}"
                            )),
                        );
                        continue;
                    }

                    let hydration_result = if let HostedWasmActor::Component(component) =
                        &actor.actor
                    {
                        self.session_snapshots
                            .iter()
                            .filter(|(known_backend, _snapshot)| {
                                actor
                                    .backend
                                    .is_none_or(|backend| backend == **known_backend)
                            })
                            .try_for_each(|(_backend, snapshot)| {
                                component
                                    .try_send_session_event(SessionEvent::Snapshot(
                                        snapshot.clone(),
                                    ))
                                    .with_context(|| {
                                        format!("hydrate init child {id:?} with session snapshot")
                                    })
                            })
                    } else {
                        Ok(())
                    };
                    if let Err(error) = hydration_result {
                        self.note_actor_failure();
                        self.stop_requested_while_starting.remove(&id);
                        if let Some(init) = self.init.as_mut() {
                            init.children.remove(&id);
                        }
                        self.failed_init_children.insert(id.clone());
                        self.spawn_actor_stop(
                            id,
                            *actor,
                            StopDisposition::StartFailed(format!("{error:#}")),
                        );
                        continue;
                    }

                    eprintln!(
                        "{}",
                        started_hosted_actor_log(
                            &id,
                            actor.actor.kind(),
                            generation,
                            actor.pane,
                            &path,
                        )
                    );
                    let panes = actor.panes.iter().copied().collect::<Vec<_>>();
                    let replaced_actor = self.actors.insert(id.clone(), *actor);
                    debug_assert!(replaced_actor.is_none());
                    drop(replaced_actor);
                    self.started_panes_awaiting_start_barrier.extend(panes);
                    self.send_init_child_event(ComponentChildEvent::Started {
                        id: id.clone(),
                        generation,
                    });
                    if self.stop_requested_while_starting.remove(&id) {
                        self.handle_init_stop(id);
                    }
                }
                ActorLifecycleCompletion::StartFailed { id, panes, message } => {
                    self.note_actor_failure();
                    self.starting_init_children.remove(&id);
                    self.discard_pending_start_messages(&id);
                    self.stop_requested_while_starting.remove(&id);
                    if let Some(init) = self.init.as_mut() {
                        init.children.remove(&id);
                    }
                    self.failed_init_children.insert(id.clone());
                    self.pending_pane_removals.extend(panes);
                    self.send_init_child_event(ComponentChildEvent::Failed { id, message });
                }
                ActorLifecycleCompletion::Stopped {
                    id,
                    pane,
                    panes,
                    disposition,
                    result,
                } => {
                    self.stopping_init_children.remove(&id);
                    if pane.is_some() {
                        self.pending_pane_removals.extend(panes);
                    } else {
                        debug_assert!(panes.is_empty());
                    }
                    match disposition {
                        StopDisposition::Requested => match result {
                            Ok(()) => {
                                eprintln!("stopped presentation init child actor id={id}");
                                self.send_init_child_event(ComponentChildEvent::Stopped { id });
                            }
                            Err(message) => {
                                self.note_actor_failure();
                                self.failed_init_children.insert(id.clone());
                                self.send_init_child_event(ComponentChildEvent::Failed {
                                    id,
                                    message,
                                });
                            }
                        },
                        StopDisposition::Failed => {
                            if let Err(message) = result {
                                eprintln!(
                                    "failed to tear down failed presentation init child \
                                     id={id}: {message}"
                                );
                            }
                        }
                        StopDisposition::StartFailed(mut message) => {
                            if let Err(cleanup) = result {
                                message = format!("{message}; teardown failed: {cleanup}");
                            }
                            self.send_init_child_event(ComponentChildEvent::Failed { id, message });
                        }
                    }
                }
            }
        }
        if !self.lifecycle_completions.is_empty()
            && let Some(proxy) = &self.event_proxy
        {
            drop(proxy.send_event(ClientEvent::ActorLifecycleReady));
        }
    }

    fn send_init_child_event(&self, event: ComponentChildEvent) {
        let Some(PresentationActor {
            actor: HostedWasmActor::Component(actor),
            ..
        }) = self.actors.get(CLIENT_INIT_ACTOR_ID)
        else {
            return;
        };
        match actor.try_send_child_event(event) {
            Ok(()) => {}
            Err(ComponentActorSendError::Full(event)) => {
                if let Err(error) = self.retain_actor_event(PendingActorEvent::Child {
                    actor: CLIENT_INIT_ACTOR_ID.to_string(),
                    event,
                }) {
                    eprintln!("failed to retain child lifecycle event: {error:#}");
                }
            }
            Err(ComponentActorSendError::Closed(_)) => {
                self.note_reliable_transition_drop();
                eprintln!("failed to deliver child lifecycle event: actor is closed");
            }
            Err(ComponentActorSendError::InvariantViolation) => {
                self.note_reliable_transition_drop();
                eprintln!("failed to deliver child lifecycle event: command invariant failed");
            }
        }
    }

    pub fn drain_host_messages(&mut self) -> Result<Vec<HostActorMessage>> {
        let mut messages = self.outbound_host_messages.drain(..).collect::<Vec<_>>();
        while let Some(message) = self.host_mailbox.try_read_envelope()? {
            messages.push(HostActorMessage {
                sender: message
                    .sender
                    .and_then(|sender| self.legacy_actor_identity(sender)),
                payload: message.payload,
            });
        }
        Ok(messages)
    }

    fn legacy_actor_identity(&self, sender: ActorId) -> Option<ActorIdentity> {
        self.legacy_actors.iter().find_map(|(actor_name, actor)| {
            actor
                .generation
                .actor_ids()
                .values()
                .any(|actor_id| *actor_id == sender)
                .then(|| ActorIdentity {
                    name: actor_name.clone(),
                    generation: sender.raw(),
                })
        })
    }

    pub fn drain_backend_messages(&mut self) -> Vec<BackendActorMessageRequest> {
        self.outbound_backend_messages.drain(..).collect()
    }

    pub fn drain_session_commands(&mut self) -> Vec<SessionCommandEnvelope> {
        self.outbound_session_commands.drain(..).collect()
    }

    pub fn broadcast_session_event(&self, event: &SessionEvent) -> Result<usize> {
        let mut delivered = 0;
        for (actor_id, actor) in &self.actors {
            if let HostedWasmActor::Component(actor) = &actor.actor {
                match actor.try_send_session_event(event.clone()) {
                    Ok(()) => {}
                    Err(ComponentActorSendError::Full(event)) => {
                        self.retain_actor_event(PendingActorEvent::Session {
                            actor: actor_id.clone(),
                            event,
                        })?;
                    }
                    Err(ComponentActorSendError::Closed(_)) => {
                        self.note_reliable_transition_drop();
                        anyhow::bail!("deliver session event to closed actor {actor_id:?}")
                    }
                    Err(ComponentActorSendError::InvariantViolation) => {
                        self.note_reliable_transition_drop();
                        anyhow::bail!(
                            "deliver session event to actor {actor_id:?}: command invariant failed"
                        )
                    }
                }
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    pub fn send_session_event(&self, actor_id: &str, event: SessionEvent) -> Result<()> {
        let actor = self
            .actors
            .get(actor_id)
            .with_context(|| format!("presentation actor {actor_id:?} is unavailable"))?;
        match &actor.actor {
            HostedWasmActor::Component(actor) => match actor.try_send_session_event(event) {
                Ok(()) => Ok(()),
                Err(ComponentActorSendError::Full(event)) => {
                    self.retain_actor_event(PendingActorEvent::Session {
                        actor: actor_id.to_string(),
                        event,
                    })
                }
                Err(ComponentActorSendError::Closed(_)) => {
                    self.note_reliable_transition_drop();
                    anyhow::bail!("component actor command queue is closed")
                }
                Err(ComponentActorSendError::InvariantViolation) => {
                    self.note_reliable_transition_drop();
                    anyhow::bail!("component actor command queue invariant failed")
                }
            },
            HostedWasmActor::Preview1(_) => {
                anyhow::bail!("Preview 1 compatibility actors cannot use session control")
            }
        }
    }

    /// Preview 1 compatibility actors do not yet expose an event callback.
    /// Component and legacy actors wake Winit when their observer queues gain
    /// work, so they must not keep a periodic client timer armed while idle.
    pub fn requires_periodic_event_poll(&self) -> bool {
        self.actors
            .values()
            .any(|actor| matches!(actor.actor, HostedWasmActor::Preview1(_)))
    }

    pub fn pane_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.actors
            .values()
            .flat_map(|actor| actor.panes.iter().copied())
    }

    pub fn pane_realms(&self) -> impl Iterator<Item = (u64, Option<(BackendId, u64)>)> + '_ {
        self.actors.values().flat_map(|actor| {
            actor
                .panes
                .iter()
                .copied()
                .map(|pane| (pane, actor.backend.zip(actor.session)))
        })
    }

    pub fn pane_actor_ids(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        self.actors.iter().flat_map(|(id, actor)| {
            actor
                .panes
                .iter()
                .copied()
                .map(move |pane| (pane, id.as_str()))
        })
    }

    pub fn close_pane(&mut self, pane: u64) -> Result<bool> {
        let Some(actor_id) = self
            .actors
            .iter()
            .find_map(|(id, actor)| actor.panes.contains(&pane).then(|| id.clone()))
        else {
            return Ok(false);
        };
        match actor_id.as_str() {
            "session-switcher" => {
                self.send_actor_message(
                    &actor_id,
                    "key-policy".to_string(),
                    br#"{"type":"sidebar-action","action":"hide"}"#.to_vec(),
                )?;
            }
            "picker" => {
                self.send_actor_message(
                    &actor_id,
                    "pane-close".to_string(),
                    br#"{"type":"picker-close"}"#.to_vec(),
                )?;
            }
            "keybindings" => {
                self.send_actor_message(
                    &actor_id,
                    "pane-close".to_string(),
                    br#"{"type":"keybindings-close"}"#.to_vec(),
                )?;
            }
            "inbox" => {}
            _ if self.is_init_child(&actor_id) => self.handle_init_stop(actor_id),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn drain_pending_pane_creations(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.pending_pane_creations.drain(..)
    }

    pub fn drain_pending_pane_removals(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.pending_pane_removals.drain(..)
    }

    pub fn send_presentation_event(&self, event: PresentationEvent) -> Result<()> {
        let key_input = matches!(&event, PresentationEvent::Key { pressed: true, .. });
        let pane = presentation_event_pane(&event)
            .context("actor-scoped render hints require explicit actor routing")?;
        let (actor_name, hosted) = self
            .actors
            .iter()
            .find(|(_name, actor)| actor.panes.contains(&pane))
            .with_context(|| format!("presentation pane {pane} has no actor"))?;
        let watermark = self.graphics.as_ref().and_then(|graphics| {
            graphics.admit_actor_event(
                &ActorScope::new(actor_name.clone(), hosted.actor.generation()),
                key_input,
            )
        });
        if std::env::var_os("BROOD_TRACE_RESIZE").is_some()
            && let PresentationEvent::PaneResized {
                pane,
                width,
                height,
                resize_epoch,
                ..
            } = &event
        {
            eprintln!(
                "resize-plugin event-enqueued actor={} generation={} pane={} resize_epoch={resize_epoch:?} logical_size={width:.2}x{height:.2} event_watermark={}",
                actor_name,
                hosted.actor.generation(),
                pane,
                watermark.unwrap_or(0),
            );
        }
        match &hosted.actor {
            HostedWasmActor::Component(actor) => {
                self.send_ordered_presentation_event(actor_name, actor, event, watermark)
            }
            HostedWasmActor::Preview1(_) => {
                anyhow::bail!("Preview 1 compatibility actors cannot own presentation panes")
            }
        }
    }

    pub fn send_render_hint(
        &self,
        actor: &ActorIdentity,
        hint: brood_wasm::presentation::RenderHint,
    ) -> Result<()> {
        let event_watermark = hint.event_watermark;
        let hosted = self
            .actors
            .get(&actor.name)
            .filter(|hosted| hosted.actor.generation() == actor.generation)
            .context("render hint actor generation is unavailable")?;
        match &hosted.actor {
            HostedWasmActor::Component(component) => self.send_ordered_presentation_event(
                &actor.name,
                component,
                PresentationEvent::RenderHint(hint),
                Some(event_watermark),
            ),
            HostedWasmActor::Preview1(_) => {
                anyhow::bail!("Preview 1 compatibility actors cannot receive render hints")
            }
        }
    }

    pub fn send_key_policy_message(&self, actor_id: &str, payload: Vec<u8>) -> Result<()> {
        if let Some(actor) = self.actors.get(actor_id) {
            return actor
                .actor
                .try_send_message("key-policy".to_string(), payload)
                .with_context(|| format!("enqueue key-policy message for actor {actor_id:?}"));
        }
        if self.legacy_actors.contains_key(actor_id) {
            return self.send_legacy_message(actor_id, payload);
        }
        if self.buffer_starting_actor_message(actor_id, "key-policy".to_string(), payload)? {
            return Ok(());
        }
        anyhow::bail!("presentation actor {actor_id:?} is unavailable")
    }

    pub fn send_layout_state_to(&self, actor: &ActorIdentity, payload: Vec<u8>) -> Result<()> {
        ensure!(
            self.actor_identity(&actor.name).as_ref() == Some(actor),
            "presentation actor {:?} generation {} does not hold layout-state authority",
            actor.name,
            actor.generation
        );
        if let Some(hosted) = self.actors.get(&actor.name) {
            return hosted
                .actor
                .try_send_message("layout-host".to_string(), payload)
                .with_context(|| {
                    format!(
                        "deliver lease-scoped layout state to component actor {:?}",
                        actor.name
                    )
                });
        }
        self.send_legacy_message(&actor.name, payload)
    }

    pub fn actor_identity(&self, actor_name: &str) -> Option<ActorIdentity> {
        if let Some(actor) = self.actors.get(actor_name) {
            return Some(ActorIdentity {
                name: actor_name.to_string(),
                generation: actor.actor.generation(),
            });
        }
        self.legacy_actors.get(actor_name).and_then(|actor| {
            actor
                .generation
                .actor_ids()
                .get(actor_name)
                .map(|actor_id| ActorIdentity {
                    name: actor_name.to_string(),
                    generation: actor_id.raw(),
                })
        })
    }

    /// Whether the authenticated actor generation can no longer receive a
    /// terminal mailbox message. A full live mailbox deliberately returns
    /// false so callers continue retrying it.
    pub fn actor_mailbox_is_closed(&self, actor: &ActorIdentity) -> bool {
        if self.actor_identity(&actor.name).as_ref() != Some(actor) {
            return true;
        }
        if let Some(hosted) = self.actors.get(&actor.name) {
            return hosted.actor.mailbox_is_closed();
        }
        let Some(legacy) = self.legacy_actors.get(&actor.name) else {
            return true;
        };
        let Some(actor_id) = legacy.generation.actor_ids().get(&actor.name).copied() else {
            return true;
        };
        legacy.mailbox.actor_id() != actor_id || legacy.mailbox.is_closed()
    }

    pub fn actor_identities(&self) -> Vec<ActorIdentity> {
        self.actors
            .keys()
            .chain(self.legacy_actors.keys())
            .filter_map(|name| self.actor_identity(name))
            .collect()
    }

    pub fn bind_session_snapshot(&mut self, snapshot: &SessionSnapshot) {
        let backend = BackendId(snapshot.backend_id);
        self.connected_backends.insert(backend);
        self.session_snapshots.insert(backend, snapshot.clone());
        self.session_workspaces
            .retain(|(known_backend, _session), _path| *known_backend != backend);
        self.session_workspaces
            .extend(snapshot.sessions.iter().map(|session| {
                let directory = session
                    .working_directory
                    .as_deref()
                    .unwrap_or(&session.project_directory);
                ((backend, session.id), PathBuf::from(directory))
            }));
    }

    pub fn disconnect_backend(&mut self, backend: BackendId) -> Result<()> {
        self.connected_backends.remove(&backend);
        let revision = self
            .session_snapshots
            .get(&backend)
            .map_or(1, |snapshot| snapshot.revision.saturating_add(1));
        let disconnected = SessionSnapshot {
            backend_id: backend.0,
            revision,
            active: 0,
            sessions: Vec::new(),
        };
        self.session_snapshots.insert(backend, disconnected.clone());
        self.session_workspaces
            .retain(|(known_backend, _session), _path| *known_backend != backend);
        self.broadcast_session_event(&SessionEvent::Snapshot(disconnected.clone()))?;
        self.stop_actors_for_closed_sessions(&disconnected);
        self.session_snapshots.remove(&backend);
        if self.bound_backend_id == Some(backend) {
            self.bound_backend_id = None;
            self.bound_session_id = None;
        }
        Ok(())
    }

    pub fn connect_backend(&mut self, backend: BackendId) {
        self.connected_backends.insert(backend);
    }

    pub fn discard_backend_requests(&mut self, backend: BackendId) {
        self.outbound_backend_messages
            .retain(|message| message.backend != backend);
        self.outbound_session_commands
            .retain(|command| command.request.backend_id != backend.0);
    }

    pub fn stop_actors_for_closed_sessions(&mut self, snapshot: &SessionSnapshot) {
        let live = snapshot
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let stale = self
            .actors
            .iter()
            .filter_map(|(id, actor)| {
                actor
                    .backend
                    .zip(actor.session)
                    .filter(|(actor_backend, session)| {
                        *actor_backend == BackendId(snapshot.backend_id) && !live.contains(session)
                    })
                    .map(|_| id.clone())
            })
            .collect::<Vec<_>>();
        for actor in stale {
            self.handle_init_stop(actor);
        }
    }

    pub fn send_actor_message(
        &self,
        actor_id: &str,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<()> {
        if let Some(actor) = self.actors.get(actor_id) {
            return actor
                .actor
                .try_send_message(sender, payload)
                .with_context(|| {
                    format!("deliver mailbox message to presentation actor {actor_id:?}")
                });
        }
        if self.legacy_actors.contains_key(actor_id) {
            return self.send_legacy_message(actor_id, payload);
        }
        if self.buffer_starting_actor_message(actor_id, sender, payload)? {
            return Ok(());
        }
        anyhow::bail!("presentation actor {actor_id:?} is unavailable")
    }

    fn buffer_starting_actor_message(
        &self,
        actor_id: &str,
        sender: String,
        payload: Vec<u8>,
    ) -> Result<bool> {
        if !self.starting_init_children.contains(actor_id) {
            return Ok(false);
        }
        let mut pending = self
            .pending_start_messages
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("pending-start message queue is poisoned"))?;
        let messages = pending.entry(actor_id.to_string()).or_default();
        ensure!(
            messages.len() < MAX_PENDING_START_MESSAGES,
            "presentation actor {actor_id:?} pending-start mailbox is full"
        );
        let retained_bytes = messages.iter().fold(0_usize, |bytes, (sender, payload)| {
            bytes
                .saturating_add(sender.len())
                .saturating_add(payload.len())
        });
        ensure!(
            retained_bytes
                .saturating_add(sender.len())
                .saturating_add(payload.len())
                <= MAX_PENDING_START_MESSAGE_BYTES,
            "presentation actor {actor_id:?} pending-start mailbox byte budget is exhausted"
        );
        messages.push_back((sender, payload));
        drop(pending);
        Ok(true)
    }

    fn take_pending_start_messages(&self, actor_id: &str) -> Result<VecDeque<PendingStartMessage>> {
        self.pending_start_messages
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("pending-start message queue is poisoned"))
            .map(|mut pending| pending.remove(actor_id).unwrap_or_default())
    }

    fn discard_pending_start_messages(&self, actor_id: &str) {
        match self.pending_start_messages.lock() {
            Ok(mut pending) => {
                pending.remove(actor_id);
            }
            Err(_poisoned) => {
                eprintln!("pending-start message queue is poisoned while removing {actor_id:?}");
            }
        }
    }

    pub fn send_backend_actor_message(
        &self,
        backend: BackendId,
        realm: BackendActorRealm,
        actor_id: &str,
        sender: &str,
        payload: Vec<u8>,
    ) -> Result<()> {
        let actor_realm = {
            let actor = self
                .actors
                .get(actor_id)
                .with_context(|| format!("presentation actor {actor_id:?} is unavailable"))?;
            actor.backend.zip(actor.session)
        };
        ensure!(
            actor_accepts_backend_message(actor_realm, backend, realm),
            "backend {} {realm:?} addressed presentation actor {actor_id:?} outside its association",
            backend.0,
        );
        let sender = match realm {
            BackendActorRealm::Daemon => format!("daemon:{}:{sender}", backend.0),
            BackendActorRealm::Session(_) => format!("session:{}:{sender}", backend.0),
        };
        self.send_actor_message(actor_id, sender, payload)
            .with_context(|| format!("deliver backend message to presentation actor {actor_id:?}"))
    }

    pub fn pane_for_actor(&self, actor: &ActorIdentity) -> Option<u64> {
        (self.actor_identity(&actor.name).as_ref() == Some(actor))
            .then(|| self.actors.get(&actor.name).map(|hosted| hosted.pane))
            .flatten()
            .filter(|pane| *pane != 0)
    }

    pub fn can_claim_root_layout(&self, actor: &ActorIdentity) -> bool {
        self.actor_identity(&actor.name).as_ref() == Some(actor) && self.is_init_child(&actor.name)
    }

    pub fn layout_actor_identities(&self) -> Vec<ActorIdentity> {
        self.legacy_actors
            .iter()
            .filter(|(_, actor)| {
                actor
                    .declaration
                    .subscribes_to(WasmActorEventKind::LayoutState)
            })
            .filter_map(|(actor_name, actor)| {
                actor
                    .generation
                    .actor_ids()
                    .get(actor_name)
                    .map(|actor_id| ActorIdentity {
                        name: actor_name.clone(),
                        generation: actor_id.raw(),
                    })
            })
            .collect()
    }

    fn send_legacy_message(&self, actor_id: &str, payload: Vec<u8>) -> Result<()> {
        self.legacy_actors
            .get(actor_id)
            .with_context(|| format!("legacy presentation actor {actor_id:?} is unavailable"))?
            .mailbox
            .try_post(payload)
    }

    pub fn shutdown(mut self) -> Result<()> {
        let mut first_error = None;
        let tasks = self.lifecycle_tasks.drain(..).collect::<Vec<_>>();
        for task in &tasks {
            task.abort();
        }
        self.executor.block_on(async {
            for task in tasks {
                let _joined = tokio::time::timeout(Duration::from_secs(1), task).await;
            }
        });
        let actors = std::mem::take(&mut self.actors);
        let actor_shutdowns = actors
            .into_iter()
            .map(|(actor_id, mut actor)| {
                self.executor.spawn(async move {
                    let result = run_bounded_actor_operation(
                        &format!("presentation actor {actor_id:?} shutdown"),
                        ACTOR_SHUTDOWN_TIMEOUT,
                        actor.actor.shutdown(),
                    )
                    .await;
                    (actor_id, result)
                })
            })
            .collect::<Vec<_>>();
        for task in actor_shutdowns {
            let result = self.executor.block_on(task);
            let (actor_id, result) = match result {
                Ok(result) => result,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow::anyhow!(
                            "join presentation actor shutdown task: {error}"
                        ));
                    }
                    continue;
                }
            };
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(anyhow::anyhow!(
                    "shut down presentation actor {actor_id:?}: {error}"
                ));
            }
        }

        let legacy_shutdowns = std::mem::take(&mut self.legacy_actors)
            .into_iter()
            .map(|(actor_id, actor)| {
                let actor_ids = actor.generation.actor_ids().clone();
                self.executor.spawn(async move {
                    let joined = tokio::time::timeout(
                        ACTOR_SHUTDOWN_TIMEOUT,
                        actor.generation.cancel_and_join(),
                    )
                    .await;
                    (actor_id, actor_ids, joined)
                })
            })
            .collect::<Vec<_>>();
        for task in legacy_shutdowns {
            let result = self.executor.block_on(task);
            let (actor_id, actor_ids, joined) = match result {
                Ok(result) => result,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow::anyhow!(
                            "join legacy presentation actor shutdown task: {error}"
                        ));
                    }
                    continue;
                }
            };
            if joined.is_err() && first_error.is_none() {
                first_error = Some(anyhow::anyhow!(
                    "legacy presentation actor {actor_id:?} shutdown exceeded {} ms and was aborted",
                    ACTOR_SHUTDOWN_TIMEOUT.as_millis()
                ));
            }
            self.executor.block_on({
                let registry = Arc::clone(&self.legacy_registry);
                async move {
                    let mut registry = registry.lock().await;
                    for (name, actor_id) in actor_ids {
                        registry.unregister_actor_generation(&name, actor_id);
                    }
                }
            });
        }
        // A completed lifecycle task may have returned its actor into the
        // completion queue just before shutdown began. Drop those actors so
        // their hard-stop fallbacks run before the Tokio runtime is retired.
        drop(self.lifecycle_completions);
        self.io_executor.shutdown_timeout(ACTOR_SHUTDOWN_TIMEOUT);
        self.executor.shutdown_timeout(ACTOR_SHUTDOWN_TIMEOUT);
        first_error.map_or(Ok(()), Err)
    }
}

fn direct_actor_start_context(
    instance_id: &str,
    generation: u64,
    workspace_root: &Path,
) -> PluginStartContext {
    PluginStartContext {
        instance_id: instance_id.to_string(),
        generation,
        reason: PluginStartReason::Initial,
        backend_id: None,
        session_id: None,
        workspace_root: workspace_root.to_string_lossy().into_owned(),
        data: vec![PluginLifecycleEntry {
            key: "realm".to_string(),
            value: "client".to_string(),
        }],
    }
}

fn wake_lifecycle_completion(proxy: Option<&EventLoopProxy<ClientEvent>>) {
    if let Some(proxy) = proxy {
        drop(proxy.send_event(ClientEvent::ActorLifecycleReady));
    }
}

async fn run_bounded_actor_operation<F, E>(
    operation: &str,
    deadline: Duration,
    future: F,
) -> std::result::Result<(), String>
where
    F: Future<Output = std::result::Result<(), E>>,
    E: std::fmt::Display,
{
    tokio::time::timeout(deadline, future)
        .await
        .map_err(|_elapsed| {
            format!(
                "{operation} exceeded {} ms and was aborted",
                deadline.as_millis()
            )
        })?
        .map_err(|error| error.to_string())
}

impl PresentationActorSpec {
    fn parse(spec: &str) -> Result<Self> {
        let (actor_spec, path) = spec
            .split_once('=')
            .context("presentation actor must use ID=PATH or ID*PANE_COUNT=PATH")?;
        let (id, pane_count) = if let Some((id, count)) = actor_spec.rsplit_once('*') {
            let pane_count = count
                .parse::<usize>()
                .context("presentation actor pane count must be an integer")?;
            ensure!(
                (1..=16).contains(&pane_count),
                "presentation actor pane count must be 1..=16"
            );
            (id, pane_count)
        } else {
            (actor_spec, 1)
        };
        ensure!(
            !id.trim().is_empty(),
            "presentation actor id must not be empty"
        );
        ensure!(
            !path.trim().is_empty(),
            "presentation actor path must not be empty"
        );
        Ok(Self {
            id: id.to_string(),
            path: PathBuf::from(path),
            pane_count,
        })
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "actor loading keeps the security grants and two executor handles explicit"
)]
async fn load_hosted_actor(
    spec: &PresentationActorSpec,
    watcher: NativeFilesystemWatcherService,
    presentation: Option<std::sync::Arc<crate::plugin_graphics::ActorPresentationHost>>,
    artifact: WasmPluginArtifact,
    actor_management: bool,
    capabilities: &[String],
    workspace_root: &Path,
    event_proxy: Option<EventLoopProxy<ClientEvent>>,
    io_runtime: Option<tokio::runtime::Handle>,
) -> Result<HostedWasmActor> {
    let root = spec.path.parent().unwrap_or_else(|| Path::new("."));
    let (filesystem, runtime_io) = presentation_actor_grants(root, workspace_root, capabilities)?;
    let actor = match artifact {
        WasmPluginArtifact::Component { core_module_error } => {
            let actor = ComponentActor::load(
                &spec.path,
                ComponentActorConfig {
                    filesystem: Some(filesystem.clone()),
                    filesystem_watcher_service: watcher,
                    runtime_io,
                    presentation: presentation.map(|presentation| {
                        presentation as std::sync::Arc<dyn brood_wasm::PresentationHost>
                    }),
                    actor_management,
                    session_messaging: true,
                    daemon_messaging: true,
                    session_control: true,
                    event_waker: event_proxy.map(|proxy| {
                        ComponentActorEventWaker::new(move || {
                            drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                        })
                    }),
                    io_runtime: io_runtime.clone(),
                    ..ComponentActorConfig::default()
                },
            )
            .await
            .with_context(|| {
                format!(
                    "{} is neither a core Brood module ({core_module_error}) nor a component actor",
                    spec.path.display()
                )
            })?;
            HostedWasmActor::Component(actor)
        }
        WasmPluginArtifact::Preview1 => {
            let actor = Preview1CompatActor::load_with_watcher_service(
                &spec.path,
                Preview1ActorCapabilities {
                    filesystem: Some(Preview1FilesystemGrant {
                        host_path: filesystem.host_path,
                        guest_path: ".".to_string(),
                        writable: filesystem.writable,
                    }),
                    runtime_io: runtime_io.capabilities,
                    io_runtime,
                },
                Preview1WorkerConfig::default(),
                watcher,
            )
            .await?;
            HostedWasmActor::Preview1(actor)
        }
        WasmPluginArtifact::Legacy(_) => {
            anyhow::bail!("legacy actor must use the core-module runtime")
        }
    };

    Ok(actor)
}

fn presentation_actor_grants(
    package_root: &Path,
    workspace_root: &Path,
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
    ensure!(
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
                    anyhow::bail!("unsupported presentation actor capability {capability:?}");
                };
                ensure!(
                    !command.is_empty()
                        && !command.contains('/')
                        && !command.contains('\\')
                        && !command.chars().any(char::is_whitespace),
                    "invalid presentation subprocess capability {capability:?}"
                );
                ensure!(
                    !commands.iter().any(|existing| existing == command),
                    "duplicate presentation subprocess capability {capability:?}"
                );
                commands.push(command.to_string());
            }
        }
    }
    ensure!(
        !allow_pty || !commands.is_empty(),
        "presentation actor PTY capability requires an allowed subprocess"
    );

    let workspace_filesystem = workspace_read || workspace_write;
    let filesystem = FilesystemGrant {
        host_path: if workspace_filesystem {
            workspace_root.to_path_buf()
        } else {
            package_root.to_path_buf()
        },
        guest_path: ".".to_string(),
        writable: workspace_write,
    };
    let runtime_io = RuntimeIoConfig {
        capabilities: RuntimeIoCapabilities {
            commands,
            allow_pty,
            allow_loopback,
        },
        // A session-associated presentation actor may represent a workspace
        // whose path exists only on a remote backend. Client-local runtimes
        // must opt into their installed package directory instead of treating
        // that remote identity as a local process cwd.
        working_directory: (!requested.is_empty()).then(|| {
            if package_cwd {
                package_root.to_path_buf()
            } else {
                workspace_root.to_path_buf()
            }
        }),
        ..RuntimeIoConfig::default()
    };
    Ok((filesystem, runtime_io))
}

const fn actor_accepts_backend_message(
    actor_realm: Option<(BackendId, u64)>,
    backend: BackendId,
    realm: BackendActorRealm,
) -> bool {
    match (actor_realm, realm) {
        (None, _) => true,
        (Some((actor_backend, actor_session)), BackendActorRealm::Session(session)) => {
            actor_backend.0 == backend.0 && actor_session == session
        }
        (Some((actor_backend, _actor_session)), BackendActorRealm::Daemon) => {
            actor_backend.0 == backend.0
        }
    }
}

const fn presentation_event_pane(event: &PresentationEvent) -> Option<u64> {
    match event {
        PresentationEvent::PaneCreated { pane, .. }
        | PresentationEvent::PaneResized { pane, .. }
        | PresentationEvent::PaneFocused { pane, .. }
        | PresentationEvent::PointerMoved { pane, .. }
        | PresentationEvent::PointerLeft { pane }
        | PresentationEvent::PointerButton { pane, .. }
        | PresentationEvent::PointerScroll { pane, .. }
        | PresentationEvent::Key { pane, .. }
        | PresentationEvent::Text { pane, .. }
        | PresentationEvent::Paste { pane, .. }
        | PresentationEvent::Image { pane, .. } => Some(*pane),
        PresentationEvent::RenderHint(_) => None,
    }
}

/// Keep one actor's host-side presentation stream causal when its component
/// command lane fills. Once any presentation event is retained, every later
/// presentation event for that actor joins the same FIFO until it drains.
/// Otherwise a newly freed component slot could admit a render hint or later
/// input ahead of the older retained event.
fn retain_after_actor_presentation_backlog(
    pending: &mut VecDeque<PendingActorEvent>,
    command: PendingActorEvent,
) -> std::result::Result<Option<PendingActorEvent>, Box<PendingActorEvent>> {
    let PendingActorEvent::Presentation { actor, .. } = &command else {
        return Ok(Some(command));
    };
    let backlogged = pending.iter().any(|pending| {
        matches!(pending, PendingActorEvent::Presentation { actor: pending_actor, .. } if pending_actor == actor)
    });
    if backlogged {
        if pending.len() >= ACTOR_OUTPUT_BACKLOG {
            return Err(Box::new(command));
        }
        pending.push_back(command);
        Ok(None)
    } else {
        Ok(Some(command))
    }
}

const fn pending_presentation_is_boundary(event: &PresentationEvent) -> bool {
    matches!(
        event,
        PresentationEvent::RenderHint(_)
            | PresentationEvent::Paste { .. }
            | PresentationEvent::Image { .. }
    )
}

fn next_flushable_actor_event(
    pending: &VecDeque<PendingActorEvent>,
    blocked_actors: &BTreeSet<String>,
    last_actor: Option<&str>,
) -> Option<usize> {
    let actors = pending
        .iter()
        .map(PendingActorEvent::actor)
        .filter(|actor| !blocked_actors.contains(*actor))
        .collect::<BTreeSet<_>>();
    let selected = last_actor
        .and_then(|last| actors.iter().copied().find(|actor| *actor > last))
        .or_else(|| actors.iter().next().copied())?;
    pending
        .iter()
        .position(|command| command.actor() == selected)
}

fn actor_output_has_capacity(
    retained_input: usize,
    events: usize,
    backend_messages: usize,
    session_commands: usize,
    host_messages: usize,
) -> bool {
    debug_assert!(retained_input <= ACTOR_OUTPUT_BACKLOG);
    events
        .saturating_add(backend_messages)
        .saturating_add(session_commands)
        .saturating_add(host_messages)
        < ACTOR_OUTPUT_BACKLOG
}

const fn observer_drain_has_more_work(
    actor_events: usize,
    total_events: usize,
    pending_observer_events: usize,
) -> bool {
    actor_events == ACTOR_OUTPUT_PER_ACTOR_QUANTUM
        || total_events == ACTOR_OUTPUT_TOTAL_QUANTUM
        || pending_observer_events > 0
}

fn rotated_actor_order<'a>(
    actors: impl Iterator<Item = &'a String>,
    last: Option<&str>,
) -> Vec<String> {
    let mut actors = actors.cloned().collect::<Vec<_>>();
    if let Some(last) = last {
        let start = actors.partition_point(|actor| actor.as_str() <= last);
        actors.rotate_left(start);
    }
    actors
}

fn started_hosted_actor_log(
    id: &str,
    kind: &str,
    generation: u64,
    pane: u64,
    path: &Path,
) -> String {
    format!(
        "started presentation WASM actor id={id} kind={kind} generation={generation} pane={pane} path={}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn legacy_address_delivers_eight_motions_and_release_while_registry_is_locked() -> Result<()> {
        let mut registry = Registry::new();
        let (registration, mut mailbox) = registry.prepare_actor("workspace", 64)?;
        let actor_id = mailbox.actor_id();
        registry.activate_prepared_actors(vec![registration], &[])?;
        let address = registry.mailbox_address(actor_id)?;
        let registry = Mutex::new(registry);
        let _registry_guard = registry
            .try_lock()
            .map_err(|error| anyhow::anyhow!("lock legacy registry fixture: {error}"))?;

        for sequence in 21_u64..=29 {
            let phase = if sequence == 29 { "release" } else { "motion" };
            address.try_post(serde_json::to_vec(&serde_json::json!({
                "event_sequence": sequence,
                "phase": phase,
            }))?)?;
        }
        for sequence in 21_u64..=29 {
            let payload = mailbox
                .try_read_message()?
                .context("pointer burst mailbox ended early")?;
            let event: serde_json::Value = serde_json::from_slice(&payload)?;
            ensure!(
                event
                    .get("event_sequence")
                    .and_then(serde_json::Value::as_u64)
                    == Some(sequence)
            );
            ensure!(event.get("phase").and_then(serde_json::Value::as_str) != Some("cancel"));
        }
        ensure!(mailbox.try_read_message()?.is_none());
        Ok(())
    }

    #[test]
    fn legacy_host_ingress_and_concurrent_outbox_are_exact_once_and_fifo() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut registry = Registry::new();
            let (registration, mut actor_mailbox) = registry.prepare_actor("workspace", 64)?;
            let actor_id = actor_mailbox.actor_id();
            registry.activate_prepared_actors(vec![registration], &[])?;
            let address = registry.mailbox_address(actor_id)?;
            let registry = Arc::new(Mutex::new(registry));
            let start = Arc::new(std::sync::Barrier::new(2));
            let actor_start = Arc::clone(&start);
            let outbox = tokio::spawn(async move {
                actor_start.wait();
                for sequence in 0_u64..10 {
                    let mut payload = vec![1];
                    payload.extend_from_slice(&sequence.to_le_bytes());
                    registry.lock().await.try_post_named_message_from(
                        actor_id,
                        "workspace",
                        payload,
                    )?;
                }
                Result::<()>::Ok(())
            });
            let host = tokio::task::spawn_blocking(move || {
                start.wait();
                for sequence in 0_u64..10 {
                    let mut payload = vec![0];
                    payload.extend_from_slice(&sequence.to_le_bytes());
                    address.try_post(payload)?;
                }
                Result::<()>::Ok(())
            });
            outbox.await??;
            host.await??;

            let mut next_host = 0_u64;
            let mut next_actor = 0_u64;
            for _message in 0..20 {
                let envelope = actor_mailbox
                    .try_read_envelope()?
                    .context("same-recipient concurrent mailbox ended early")?;
                let (source, sequence) = envelope
                    .payload
                    .split_first()
                    .context("same-recipient payload has no source")?;
                let sequence = u64::from_le_bytes(sequence.try_into()?);
                match *source {
                    0 => {
                        ensure!(envelope.sender.is_none());
                        ensure!(sequence == next_host);
                        next_host += 1;
                    }
                    1 => {
                        ensure!(envelope.sender == Some(actor_id));
                        ensure!(sequence == next_actor);
                        next_actor += 1;
                    }
                    source => anyhow::bail!("unexpected same-recipient source {source}"),
                }
            }
            ensure!(next_host == 10 && next_actor == 10);
            match actor_mailbox.try_read_envelope() {
                Ok(None) | Err(_) => {}
                Ok(Some(_extra)) => anyhow::bail!("same-recipient mailbox duplicated a message"),
            }
            Ok(())
        })
    }

    fn pending_key(actor: &str, physical_key: u32, watermark: u64) -> PendingActorEvent {
        PendingActorEvent::Presentation {
            actor: actor.to_owned(),
            event: PresentationEvent::Key {
                pane: 1,
                physical_key,
                logical_key: physical_key.to_string(),
                pressed: true,
                repeat: false,
                modifiers: 0,
            },
            watermark: Some(watermark),
        }
    }

    fn pending_hint(actor: &str, watermark: u64) -> PendingActorEvent {
        PendingActorEvent::Presentation {
            actor: actor.to_owned(),
            event: PresentationEvent::RenderHint(brood_wasm::presentation::RenderHint {
                token: brood_wasm::presentation::RenderToken(watermark),
                target_presentation_time_ns: 10,
                submission_deadline_ns: 9,
                relevant_panes: vec![1],
                event_watermark: watermark,
            }),
            watermark: Some(watermark),
        }
    }

    fn requested_host_message(index: usize) -> ComponentActorEvent {
        ComponentActorEvent::HostMessageRequested {
            payload: index.to_string().into_bytes(),
        }
    }

    #[test]
    fn bounded_input_quantum_exposes_the_65th_retained_event() {
        let mut pending = (0..=ACTOR_INPUT_FLUSH_QUANTUM)
            .map(|index| pending_key("actor", index as u32, index as u64))
            .collect::<VecDeque<_>>();
        for _ in 0..ACTOR_INPUT_FLUSH_QUANTUM {
            assert!(pending.pop_front().is_some());
        }
        assert_eq!(pending.len(), 1);
        assert!(pending.pop_front().is_some());
        assert!(pending.is_empty());
    }

    #[test]
    fn bounded_observer_quantums_expose_33rd_and_129th_host_messages() {
        let mut per_actor = (0..=ACTOR_OUTPUT_PER_ACTOR_QUANTUM)
            .map(requested_host_message)
            .collect::<VecDeque<_>>();
        for _ in 0..ACTOR_OUTPUT_PER_ACTOR_QUANTUM {
            assert!(matches!(
                per_actor.pop_front(),
                Some(ComponentActorEvent::HostMessageRequested { .. })
            ));
        }
        assert!(observer_drain_has_more_work(
            ACTOR_OUTPUT_PER_ACTOR_QUANTUM,
            ACTOR_OUTPUT_PER_ACTOR_QUANTUM,
            per_actor.len(),
        ));
        assert!(per_actor.pop_front().is_some());
        assert!(!observer_drain_has_more_work(1, 1, per_actor.len()));

        let mut total = (0..=ACTOR_OUTPUT_TOTAL_QUANTUM)
            .map(requested_host_message)
            .collect::<VecDeque<_>>();
        for _ in 0..ACTOR_OUTPUT_TOTAL_QUANTUM {
            assert!(total.pop_front().is_some());
        }
        assert!(observer_drain_has_more_work(
            1,
            ACTOR_OUTPUT_TOTAL_QUANTUM,
            total.len(),
        ));
        assert!(total.pop_front().is_some());
        assert!(!observer_drain_has_more_work(1, 1, total.len()));
    }

    #[test]
    fn actor_spec_supports_a_bounded_multi_pane_owner() -> Result<()> {
        let spec = PresentationActorSpec::parse("dashboard*2=/tmp/dashboard.wasm")?;
        ensure!(spec.id == "dashboard");
        ensure!(spec.pane_count == 2);
        ensure!(PresentationActorSpec::parse("dashboard*0=/tmp/dashboard.wasm").is_err());
        ensure!(PresentationActorSpec::parse("dashboard*17=/tmp/dashboard.wasm").is_err());
        Ok(())
    }

    #[test]
    fn hosted_actor_start_log_preserves_dynamic_actor_observability_contract() {
        assert_eq!(
            started_hosted_actor_log(
                "terminal-ui",
                "wasm-component",
                7,
                u64::MAX - 5,
                Path::new("/nix/store/example/terminal-ui.wasm"),
            ),
            format!(
                "started presentation WASM actor id=terminal-ui kind=wasm-component generation=7 pane={} path=/nix/store/example/terminal-ui.wasm",
                u64::MAX - 5
            )
        );
    }

    #[test]
    fn retained_actor_input_does_not_starve_bounded_actor_output() {
        assert!(actor_output_has_capacity(ACTOR_OUTPUT_BACKLOG, 0, 0, 0, 0));
        assert!(!actor_output_has_capacity(
            ACTOR_OUTPUT_BACKLOG,
            ACTOR_OUTPUT_BACKLOG,
            0,
            0,
            0
        ));
    }

    #[test]
    fn retained_presentation_fifo_orders_cutoff_hint_before_later_input() {
        let mut pending = VecDeque::from([pending_key("actor-a", 1, 1)]);
        assert!(matches!(
            retain_after_actor_presentation_backlog(&mut pending, pending_hint("actor-a", 1)),
            Ok(None)
        ));
        assert!(matches!(
            retain_after_actor_presentation_backlog(&mut pending, pending_key("actor-a", 2, 2)),
            Ok(None)
        ));
        assert!(matches!(
            retain_after_actor_presentation_backlog(
                &mut pending,
                pending_key("actor-b", 3, 1)
            ),
            Ok(Some(PendingActorEvent::Presentation { actor, .. })) if actor == "actor-b"
        ));
        assert!(matches!(
            pending.front(),
            Some(PendingActorEvent::Presentation {
                event: PresentationEvent::Key {
                    physical_key: 1,
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            pending.get(1),
            Some(PendingActorEvent::Presentation {
                event: PresentationEvent::RenderHint(_),
                ..
            })
        ));
        assert!(matches!(
            pending.get(2),
            Some(PendingActorEvent::Presentation {
                event: PresentationEvent::Key {
                    physical_key: 2,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn full_actor_does_not_block_retained_fifo_progress_for_another_actor() {
        let pending = VecDeque::from([
            pending_key("full-a", 1, 1),
            pending_key("full-a", 2, 2),
            pending_key("ready-b", 3, 1),
        ]);
        let blocked = BTreeSet::from(["full-a".to_owned()]);
        assert_eq!(
            next_flushable_actor_event(&pending, &blocked, None),
            Some(2)
        );
        assert_eq!(
            pending.get(2).map(PendingActorEvent::actor),
            Some("ready-b")
        );
        assert_eq!(
            pending
                .iter()
                .take(2)
                .map(PendingActorEvent::actor)
                .collect::<Vec<_>>(),
            vec!["full-a", "full-a"]
        );
    }

    #[test]
    fn rotating_retained_input_cursor_reaches_actor_beyond_one_full_quantum() {
        let pending = (0..=ACTOR_INPUT_FLUSH_QUANTUM)
            .map(|index| pending_key(&format!("full-{index:03}"), index as u32, 1))
            .chain([pending_key("ready-z", 999, 1)])
            .collect::<VecDeque<_>>();
        let mut blocked = BTreeSet::new();
        let mut last = None;
        for _ in 0..ACTOR_INPUT_FLUSH_QUANTUM {
            let index = next_flushable_actor_event(&pending, &blocked, last.as_deref());
            let actor = index
                .and_then(|index| pending.get(index))
                .map(PendingActorEvent::actor);
            assert!(actor.is_some_and(|actor| actor.starts_with("full-")));
            let actor = actor.unwrap_or_default().to_owned();
            blocked.insert(actor.clone());
            last = Some(actor);
        }

        // A new flush clears only the per-turn blocked set. Its persistent
        // cursor tries the 65th full actor, then advances to ready-z instead
        // of retrying the same first 64 forever.
        blocked.clear();
        let next = next_flushable_actor_event(&pending, &blocked, last.as_deref())
            .and_then(|index| pending.get(index))
            .map(PendingActorEvent::actor);
        assert_eq!(next, Some("full-064"));
        blocked.insert("full-064".to_owned());
        last = Some("full-064".to_owned());
        let ready = next_flushable_actor_event(&pending, &blocked, last.as_deref())
            .and_then(|index| pending.get(index))
            .map(PendingActorEvent::actor);
        assert_eq!(ready, Some("ready-z"));
    }

    #[test]
    fn reliable_transitions_and_coalesced_state_remain_bounded() {
        let mut reliable = VecDeque::new();
        for sequence in 0..ACTOR_OUTPUT_BACKLOG {
            if reliable.len() < ACTOR_OUTPUT_BACKLOG {
                reliable.push_back(sequence);
            }
        }
        assert_eq!(reliable.len(), ACTOR_OUTPUT_BACKLOG);
        assert!(actor_output_has_capacity(reliable.len(), 0, 0, 0, 0));
        assert!(!actor_output_has_capacity(
            reliable.len(),
            ACTOR_OUTPUT_BACKLOG,
            0,
            0,
            0
        ));

        let mut coalesced_pointer_state = BTreeMap::new();
        for position in 0..ACTOR_OUTPUT_BACKLOG.saturating_mul(4) {
            coalesced_pointer_state.insert(("actor", 7_u64), position);
        }
        assert_eq!(coalesced_pointer_state.len(), 1);
        assert_eq!(
            coalesced_pointer_state.get(&("actor", 7)),
            Some(&(ACTOR_OUTPUT_BACKLOG * 4 - 1))
        );
        assert_eq!(
            reliable.into_iter().collect::<Vec<_>>(),
            (0..ACTOR_OUTPUT_BACKLOG).collect::<Vec<_>>()
        );
    }

    #[test]
    fn bounded_actor_output_rotation_gives_every_busy_actor_progress() {
        let actors = (0..9)
            .map(|index| format!("actor-{index}"))
            .collect::<BTreeSet<_>>();
        let first = rotated_actor_order(actors.iter(), None);
        let second = rotated_actor_order(actors.iter(), first.get(3).map(String::as_str));
        let third = rotated_actor_order(actors.iter(), second.get(3).map(String::as_str));
        let progressed = first
            .into_iter()
            .take(4)
            .chain(second.into_iter().take(4))
            .chain(third.into_iter().take(4))
            .collect::<BTreeSet<_>>();
        assert_eq!(progressed, actors);
    }

    #[test]
    fn stuck_actor_teardown_is_scheduled_without_blocking_the_caller() -> Result<()> {
        let executor = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()?;
        let caller_started = Instant::now();
        let task = executor.spawn(async {
            run_bounded_actor_operation(
                "test actor shutdown",
                Duration::from_millis(100),
                std::future::pending::<std::result::Result<(), &'static str>>(),
            )
            .await
        });
        ensure!(
            caller_started.elapsed() < Duration::from_millis(50),
            "scheduling teardown blocked the event-loop-facing caller"
        );
        let result = executor.block_on(task)?;
        let Err(error) = result else {
            anyhow::bail!("stuck teardown unexpectedly succeeded");
        };
        ensure!(error.contains("exceeded 100 ms and was aborted"));
        Ok(())
    }

    #[test]
    fn normal_actor_teardown_completes_before_its_deadline() -> Result<()> {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        executor
            .block_on(run_bounded_actor_operation(
                "test actor shutdown",
                Duration::from_secs(1),
                std::future::ready(Ok::<(), &'static str>(())),
            ))
            .map_err(anyhow::Error::msg)
    }

    #[test]
    fn direct_component_start_context_preserves_each_instance_identity() {
        let root = Path::new("/workspace");
        let fast = direct_actor_start_context("resize-proof-fast", 7, root);
        let slow = direct_actor_start_context("resize-proof-slow", 8, root);
        assert_eq!(fast.instance_id, "resize-proof-fast");
        assert_eq!(slow.instance_id, "resize-proof-slow");
        assert_ne!(fast.instance_id, slow.instance_id);
        assert_eq!(fast.generation, 7);
        assert_eq!(slow.generation, 8);
        assert_eq!(fast.backend_id, None);
        assert_eq!(slow.session_id, None);
        assert_eq!(fast.workspace_root, "/workspace");
        assert_eq!(fast.data, slow.data);
    }

    #[test]
    fn presentation_child_git_capability_keeps_package_filesystem_read_only() -> Result<()> {
        let package = Path::new("/package");
        let workspace = Path::new("/workspace");
        let (filesystem, runtime) =
            presentation_actor_grants(package, workspace, &["subprocess:git".to_string()])?;
        ensure!(filesystem.host_path == package);
        ensure!(!filesystem.writable);
        ensure!(runtime.capabilities.commands == ["git"]);
        ensure!(runtime.working_directory.as_deref() == Some(workspace));
        Ok(())
    }

    #[test]
    fn presentation_child_workspace_grants_are_explicit() -> Result<()> {
        let (filesystem, _) = presentation_actor_grants(
            Path::new("/package"),
            Path::new("/workspace"),
            &["filesystem:workspace-read".to_string()],
        )?;
        ensure!(filesystem.host_path == Path::new("/workspace"));
        ensure!(!filesystem.writable);
        Ok(())
    }

    #[test]
    fn presentation_child_can_run_a_client_local_package_runtime() -> Result<()> {
        let (filesystem, runtime) = presentation_actor_grants(
            Path::new("/package"),
            Path::new("/remote/workspace"),
            &[
                "runtime:package-cwd".to_string(),
                "subprocess:node".to_string(),
            ],
        )?;
        ensure!(filesystem.host_path == Path::new("/package"));
        ensure!(runtime.working_directory.as_deref() == Some(Path::new("/package")));
        ensure!(runtime.capabilities.commands == ["node"]);
        Ok(())
    }

    #[test]
    fn presentation_child_rejects_unknown_capabilities() {
        assert!(
            presentation_actor_grants(
                Path::new("/package"),
                Path::new("/workspace"),
                &["display:ambient".to_string()],
            )
            .is_err()
        );
    }

    #[test]
    fn unbound_client_actor_accepts_messages_from_each_backend_realm() {
        assert!(actor_accepts_backend_message(
            None,
            BackendId(1),
            BackendActorRealm::Session(7)
        ));
        assert!(actor_accepts_backend_message(
            None,
            BackendId(2),
            BackendActorRealm::Daemon
        ));
    }

    #[test]
    fn realm_bound_client_actor_rejects_other_backends_and_sessions() {
        let actor_realm = Some((BackendId(1), 7));
        assert!(actor_accepts_backend_message(
            actor_realm,
            BackendId(1),
            BackendActorRealm::Session(7)
        ));
        assert!(!actor_accepts_backend_message(
            actor_realm,
            BackendId(2),
            BackendActorRealm::Session(7)
        ));
        assert!(!actor_accepts_backend_message(
            actor_realm,
            BackendId(1),
            BackendActorRealm::Session(8)
        ));
        assert!(actor_accepts_backend_message(
            actor_realm,
            BackendId(1),
            BackendActorRealm::Daemon
        ));
    }
}
