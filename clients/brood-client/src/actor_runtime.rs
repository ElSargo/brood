use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use brood_protocol::{BackendActorRealm, BackendId};
use brood_wasm::{
    ActorId, ComponentActor, ComponentActorConfig, ComponentActorEvent, ComponentActorEventWaker,
    ComponentChildEvent, FilesystemGrant, HostedWasmActor, LegacyWasmActorGeneration,
    LegacyWasmRuntimeEvent, Mailbox, NativeFilesystemWatcherService, PluginLifecycleEntry,
    PluginLifecycleEvent, PluginStartContext, PluginStartReason, PresentationEvent,
    PresentationScope, Preview1ActorCapabilities, Preview1CompatActor, Preview1FilesystemGrant,
    Preview1WorkerConfig, Registry, RuntimeIoCapabilities, RuntimeIoConfig, SessionCommandRequest,
    SessionEvent, SessionSnapshot, WasmActor, WasmActorDeclaration, WasmActorEventKind,
    WasmPluginArtifact,
};
use tokio::sync::Mutex;
use winit::event_loop::EventLoopProxy;

use crate::{ClientEvent, plugin_graphics::PluginGraphicsBridge};

const CLIENT_INIT_ACTOR_ID: &str = "brood.client-init";

#[derive(Clone, Debug, Eq, PartialEq)]
struct PresentationActorSpec {
    id: String,
    path: PathBuf,
}

pub struct PresentationWasmRuntime {
    executor: tokio::runtime::Runtime,
    actors: BTreeMap<String, PresentationActor>,
    legacy_actors: BTreeMap<String, LegacyPresentationActor>,
    legacy_registry: Arc<Mutex<Registry>>,
    host_mailbox: Mailbox,
    filesystem_watcher_service: NativeFilesystemWatcherService,
    event_proxy: Option<EventLoopProxy<ClientEvent>>,
    graphics: Option<PluginGraphicsBridge>,
    init: Option<PresentationInitState>,
    failed_init_children: BTreeSet<String>,
    outbound_backend_messages: VecDeque<BackendActorMessageRequest>,
    outbound_session_commands: VecDeque<SessionCommandEnvelope>,
    outbound_host_messages: VecDeque<HostActorMessage>,
    pending_pane_creations: VecDeque<u64>,
    pending_pane_removals: VecDeque<u64>,
    bound_backend_id: Option<BackendId>,
    bound_session_id: Option<u64>,
    connected_backends: BTreeSet<BackendId>,
    next_actor_pane: u64,
    workspace_root: PathBuf,
    session_workspaces: BTreeMap<(BackendId, u64), PathBuf>,
    session_snapshots: BTreeMap<BackendId, SessionSnapshot>,
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
    backend: Option<BackendId>,
    session: Option<u64>,
    actor: HostedWasmActor,
}

struct LegacyPresentationActor {
    declaration: WasmActorDeclaration,
    generation: LegacyWasmActorGeneration,
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
        let executor = tokio::runtime::Builder::new_multi_thread()
            .thread_name("brood-client-wasm")
            .enable_all()
            .build()
            .context("start presentation WASM executor")?;
        let mut registry = Registry::new();
        let host_mailbox = registry
            .register_host_mailbox(64)
            .context("register presentation host actor mailbox")?;
        let mut runtime = Self {
            executor,
            actors: BTreeMap::new(),
            legacy_actors: BTreeMap::new(),
            legacy_registry: Arc::new(Mutex::new(registry)),
            host_mailbox,
            filesystem_watcher_service: NativeFilesystemWatcherService::new(),
            event_proxy,
            graphics: graphics.cloned(),
            init: None,
            failed_init_children: BTreeSet::new(),
            outbound_backend_messages: VecDeque::new(),
            outbound_session_commands: VecDeque::new(),
            outbound_host_messages: VecDeque::new(),
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
        };
        if let Some(path) = init_actor {
            runtime.load_init_actor(path, bound_session, graphics)?;
        }
        for spec in specs {
            let pane = runtime.allocate_actor_pane()?;
            let id = spec.id.clone();
            runtime.load_actor(spec, pane, None, None, graphics, &[])?;
            runtime.start_direct_component_actor(&id)?;
        }
        Ok(runtime)
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

    /// Install one verified network artifact without granting management or
    /// runtime-I/O capabilities.
    pub fn install_artifact_actor(&mut self, id: &str, path: &Path) -> Result<u64> {
        ensure!(
            !id.trim().is_empty() && id != CLIENT_INIT_ACTOR_ID,
            "remote presentation actor id is unavailable"
        );
        let pane = self.allocate_actor_pane()?;
        self.load_actor(
            PresentationActorSpec {
                id: id.to_string(),
                path: path.to_path_buf(),
            },
            pane,
            None,
            None,
            self.graphics.clone().as_ref(),
            &[],
        )?;
        self.pending_pane_creations.push_back(pane);
        self.drain_events()?;
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
        reason = "the loaded actor is intentionally transferred into the long-lived client runtime"
    )]
    fn load_actor(
        &mut self,
        spec: PresentationActorSpec,
        pane: u64,
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
                pane,
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
        self.executor.block_on({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .lock()
                    .await
                    .activate_prepared_actors(vec![registration], &[])
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
            },
        );
        Ok(())
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the loaded actor is intentionally transferred into the long-lived client runtime"
    )]
    fn load_hosted_actor(
        &mut self,
        spec: PresentationActorSpec,
        pane: u64,
        backend: Option<BackendId>,
        session: Option<u64>,
        graphics: Option<&PluginGraphicsBridge>,
        artifact: WasmPluginArtifact,
        capabilities: &[String],
    ) -> Result<()> {
        let watcher = self.filesystem_watcher_service.clone();
        let presentation = graphics.map(|graphics| graphics.actor_host(pane));
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
        ))?;
        if let Some(presentation) = presentation {
            presentation.bind_generation(actor.generation());
        }
        eprintln!(
            "started presentation WASM actor id={} kind={} generation={} pane={} path={}",
            spec.id,
            actor.kind(),
            actor.generation(),
            pane,
            spec.path.display()
        );
        self.actors.insert(
            spec.id,
            PresentationActor {
                pane,
                backend,
                session,
                actor,
            },
        );
        Ok(())
    }

    pub fn drain_events(&mut self) -> Result<()> {
        let graphics = self.graphics.clone();
        self.drain_events_with_graphics(graphics.as_ref())
    }

    fn drain_events_with_graphics(
        &mut self,
        graphics: Option<&PluginGraphicsBridge>,
    ) -> Result<()> {
        let mut events = Vec::new();
        for (actor_id, hosted) in &mut self.actors {
            match &mut hosted.actor {
                HostedWasmActor::Component(actor) => {
                    while let Some(event) = actor.try_recv_event() {
                        events.push((actor_id.clone(), Ok(event)));
                    }
                }
                HostedWasmActor::Preview1(actor) => {
                    if let Some(message) = actor.try_take_failure() {
                        events.push((actor_id.clone(), Err(message)));
                    }
                }
            }
        }
        for (actor_id, event) in events {
            let event = match event {
                Ok(event) => event,
                Err(message) if self.is_init_child(&actor_id) => {
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
                    anyhow::bail!("presentation actor {actor_id:?} dispatcher failed: {message}")
                }
                ComponentActorEvent::WorkerFailed { worker, message } => anyhow::bail!(
                    "presentation actor {actor_id:?} worker {} failed: {message}",
                    worker.raw()
                ),
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
                    let sender = self.actors.get(&actor_id).with_context(|| {
                        format!("presentation actor {actor_id:?} is unavailable")
                    })?;
                    let sender_realm = sender.backend.zip(sender.session);
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
                    ensure!(
                        self.outbound_backend_messages.len() < 64,
                        "presentation actor backend-message queue is full"
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
                    ensure!(
                        self.outbound_backend_messages.len() < 64,
                        "presentation actor backend-message queue is full"
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
                    let sender = self.actors.get(&actor_id).with_context(|| {
                        format!("presentation actor {actor_id:?} is unavailable")
                    })?;
                    if let Some(sender_backend) = sender.backend {
                        ensure!(
                            sender_backend == backend,
                            "presentation actor {actor_id:?} requested a different backend"
                        );
                    }
                    if let (Some(sender_session), Some(requested_session)) =
                        (sender.session, request.session)
                    {
                        ensure!(
                            sender_session == requested_session,
                            "presentation actor {actor_id:?} requested a different session"
                        );
                    }
                    ensure!(
                        self.outbound_session_commands.len() < 64,
                        "presentation session-command queue is full"
                    );
                    eprintln!(
                        "queued presentation actor session command actor={} request={} operation={:?}",
                        actor_id, request.request, request.operation
                    );
                    self.outbound_session_commands
                        .push_back(SessionCommandEnvelope { actor_id, request });
                }
                ComponentActorEvent::HostMessageRequested { payload } => {
                    ensure!(
                        self.outbound_host_messages.len() < 64,
                        "presentation actor host-message queue is full"
                    );
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
        Ok(())
    }

    fn is_init_child(&self, id: &str) -> bool {
        self.init
            .as_ref()
            .is_some_and(|init| init.children.contains(id))
            || self.failed_init_children.contains(id)
    }

    fn handle_init_start(
        &mut self,
        id: String,
        guest_path: &str,
        capabilities: &[String],
        presentation_scope: PresentationScope,
        graphics: Option<&PluginGraphicsBridge>,
    ) {
        let result =
            self.start_init_child(&id, guest_path, capabilities, presentation_scope, graphics);
        match result {
            Ok(generation) => {
                self.send_init_child_event(ComponentChildEvent::Started { id, generation });
            }
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
    ) -> Result<u64> {
        ensure!(
            !id.trim().is_empty(),
            "init child actor id must not be empty"
        );
        ensure!(
            id != CLIENT_INIT_ACTOR_ID,
            "init actor cannot replace itself"
        );
        ensure!(
            !self.actors.contains_key(id) || self.is_init_child(id),
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
        };
        ensure!(
            !matches!(
                WasmPluginArtifact::load(&spec.path)?,
                WasmPluginArtifact::Legacy(_)
            ),
            "init child actors must use the hosted Component or Preview 1 runtime"
        );
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
        self.load_actor(spec, pane, backend, session, graphics, capabilities)?;
        let generation = self
            .actors
            .get(id)
            .context("init child did not produce a hosted actor")?
            .actor
            .generation();
        let lifecycle = PluginLifecycleEvent::Start(PluginStartContext {
            instance_id: id.to_string(),
            generation,
            reason: PluginStartReason::Initial,
            backend_id: backend.map(|backend| backend.0),
            session_id: session,
            workspace_root: self
                .session_workspaces
                .get(&realm.unwrap_or((BackendId(0), 0)))
                .unwrap_or(&self.workspace_root)
                .to_string_lossy()
                .into_owned(),
            data: vec![PluginLifecycleEntry {
                key: "realm".to_string(),
                value: "client".to_string(),
            }],
        });
        if let Err(error) = self.executor.block_on(
            self.actors
                .get(id)
                .context("init child did not produce a hosted actor")?
                .actor
                .lifecycle(lifecycle, Duration::from_secs(2)),
        ) {
            if let Some(mut actor) = self.actors.remove(id)
                && let Err(shutdown_error) = self.executor.block_on(actor.actor.shutdown())
            {
                eprintln!(
                    "failed to shut down init child after lifecycle failure id={id}: \
                     {shutdown_error:#}"
                );
            }
            return Err(error).context("init child start lifecycle failed");
        }
        if let Some(init) = self.init.as_mut() {
            init.children.insert(id.to_string());
        }
        self.failed_init_children.remove(id);
        if let HostedWasmActor::Component(actor) = &self
            .actors
            .get(id)
            .context("init child did not produce a hosted actor")?
            .actor
        {
            for snapshot in self
                .session_snapshots
                .iter()
                .filter(|(known_backend, _snapshot)| {
                    backend.is_none_or(|backend| backend == **known_backend)
                })
                .map(|(_backend, snapshot)| snapshot)
            {
                actor
                    .try_send_session_event(SessionEvent::Snapshot(snapshot.clone()))
                    .with_context(|| format!("hydrate init child {id:?} with session snapshot"))?;
            }
        }
        self.pending_pane_creations.push_back(pane);
        Ok(generation)
    }

    fn handle_init_stop(&mut self, id: String) {
        if !self.is_init_child(&id) {
            self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: "init actor may stop only its own children".to_string(),
            });
            return;
        }
        let mut removed_pane = None;
        let result = self.actors.remove(&id).map_or_else(
            || Err(anyhow::anyhow!("init child actor is unavailable")),
            |mut actor| {
                removed_pane = (actor.pane != 0).then_some(actor.pane);
                let lifecycle = PluginLifecycleEvent::Stop(brood_wasm::PluginStopContext {
                    instance_id: id.clone(),
                    generation: actor.actor.generation(),
                    reason: brood_wasm::PluginStopReason::Shutdown,
                    next_generation: None,
                });
                let lifecycle_result = self
                    .executor
                    .block_on(actor.actor.lifecycle(lifecycle, Duration::from_secs(2)));
                let shutdown_result = self.executor.block_on(actor.actor.shutdown());
                lifecycle_result.and(shutdown_result)
            },
        );
        if let Some(init) = self.init.as_mut() {
            init.children.remove(&id);
        }
        self.failed_init_children.remove(&id);
        match result {
            Ok(()) => {
                if let Some(pane) = removed_pane {
                    self.pending_pane_removals.push_back(pane);
                }
                eprintln!("stopped presentation init child actor id={id}");
                self.send_init_child_event(ComponentChildEvent::Stopped { id });
                if let Some(proxy) = &self.event_proxy {
                    drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
                }
            }
            Err(error) => self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: format!("{error:#}"),
            }),
        }
    }

    fn remove_failed_init_child(&mut self, id: &str) {
        let Some(mut actor) = self.actors.remove(id) else {
            return;
        };
        if let Some(init) = self.init.as_mut() {
            init.children.remove(id);
        }
        self.failed_init_children.insert(id.to_string());
        if actor.pane != 0 {
            self.pending_pane_removals.push_back(actor.pane);
        }
        if let Err(error) = self.executor.block_on(actor.actor.shutdown()) {
            eprintln!("joined failed presentation init child id={id}: {error:#}");
        }
        if let Some(proxy) = &self.event_proxy {
            drop(proxy.send_event(ClientEvent::ActorHostMessagesReady));
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
        if let Err(error) = actor.try_send_child_event(event) {
            eprintln!("failed to deliver child lifecycle event to client init actor: {error:#}");
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
                actor
                    .try_send_session_event(event.clone())
                    .with_context(|| format!("deliver session event to actor {actor_id:?}"))?;
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
            HostedWasmActor::Component(actor) => actor.try_send_session_event(event),
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
            .filter(|actor| actor.pane != 0)
            .map(|actor| actor.pane)
    }

    pub fn pane_realms(&self) -> impl Iterator<Item = (u64, Option<(BackendId, u64)>)> + '_ {
        self.actors
            .values()
            .filter(|actor| actor.pane != 0)
            .map(|actor| (actor.pane, actor.backend.zip(actor.session)))
    }

    pub fn pane_actor_ids(&self) -> impl Iterator<Item = (u64, &str)> + '_ {
        self.actors
            .iter()
            .filter(|(_id, actor)| actor.pane != 0)
            .map(|(id, actor)| (actor.pane, id.as_str()))
    }

    pub fn close_pane(&mut self, pane: u64) -> Result<bool> {
        let Some(actor_id) = self
            .actors
            .iter()
            .find_map(|(id, actor)| (actor.pane == pane).then(|| id.clone()))
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
        let pane = presentation_event_pane(&event);
        let actor = self
            .actors
            .values()
            .find(|actor| actor.pane == pane)
            .with_context(|| format!("presentation pane {pane} has no actor"))?;
        match &actor.actor {
            HostedWasmActor::Component(actor) => actor.try_send_presentation_event(event),
            HostedWasmActor::Preview1(_) => {
                anyhow::bail!("Preview 1 compatibility actors cannot own presentation panes")
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
        ensure!(
            self.legacy_actors.contains_key(actor_id),
            "presentation actor {actor_id:?} is unavailable"
        );
        self.send_legacy_message(actor_id, payload)
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
        self.executor.block_on({
            let registry = Arc::clone(&self.legacy_registry);
            let actor_name = actor.name.clone();
            async move {
                registry
                    .lock()
                    .await
                    .actor_mailbox_is_closed(&actor_name, actor_id)
            }
        })
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
        anyhow::bail!("presentation actor {actor_id:?} is unavailable")
    }

    pub fn send_backend_actor_message(
        &self,
        backend: BackendId,
        realm: BackendActorRealm,
        actor_id: &str,
        sender: &str,
        payload: Vec<u8>,
    ) -> Result<()> {
        let actor = self
            .actors
            .get(actor_id)
            .with_context(|| format!("presentation actor {actor_id:?} is unavailable"))?;
        let actor_realm = actor.backend.zip(actor.session);
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
        self.executor.block_on({
            let registry = Arc::clone(&self.legacy_registry);
            let actor_id = actor_id.to_string();
            async move {
                registry
                    .lock()
                    .await
                    .try_post_named_message(&actor_id, payload)
            }
        })
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let actor_ids = self.actors.keys().cloned().collect::<Vec<_>>();
        let mut first_error = None;
        for actor_id in actor_ids {
            let Some(mut actor) = self.actors.remove(&actor_id) else {
                continue;
            };
            if let Err(error) = self.executor.block_on(actor.actor.shutdown())
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }

        for (_actor_id, actor) in std::mem::take(&mut self.legacy_actors) {
            let actor_ids = actor.generation.actor_ids().clone();
            self.executor.block_on(actor.generation.cancel_and_join());
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

impl PresentationActorSpec {
    fn parse(spec: &str) -> Result<Self> {
        let (id, path) = spec
            .split_once('=')
            .context("presentation actor must use ID=PATH")?;
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
        })
    }
}

async fn load_hosted_actor(
    spec: &PresentationActorSpec,
    watcher: NativeFilesystemWatcherService,
    presentation: Option<std::sync::Arc<crate::plugin_graphics::ActorPresentationHost>>,
    artifact: WasmPluginArtifact,
    actor_management: bool,
    capabilities: &[String],
    workspace_root: &Path,
    event_proxy: Option<EventLoopProxy<ClientEvent>>,
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

const fn presentation_event_pane(event: &PresentationEvent) -> u64 {
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
        | PresentationEvent::Image { pane, .. } => *pane,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
