use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use brood_wasm::{
    ActorId, ComponentActor, ComponentActorConfig, ComponentActorEvent, ComponentChildEvent,
    FilesystemGrant, HostedWasmActor, LegacyWasmActorGeneration, LegacyWasmRuntimeEvent, Mailbox,
    NativeFilesystemWatcherService, PluginLifecycleEntry, PluginLifecycleEvent, PluginStartContext,
    PluginStartReason, PresentationEvent, Preview1ActorCapabilities, Preview1CompatActor,
    Preview1FilesystemGrant, Preview1WorkerConfig, Registry, RuntimeIoCapabilities,
    RuntimeIoConfig, SessionCommandRequest, SessionEvent, WasmActor, WasmActorDeclaration,
    WasmActorEventKind, WasmPluginArtifact,
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
    outbound_session_messages: VecDeque<SessionActorMessageRequest>,
    outbound_session_commands: VecDeque<SessionCommandEnvelope>,
    outbound_host_messages: VecDeque<HostActorMessage>,
    pending_pane_creations: VecDeque<u64>,
    bound_session_id: Option<u64>,
    next_actor_pane: u64,
}

pub struct SessionActorMessageRequest {
    pub sender: String,
    pub session_id: u64,
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
        session_id: Option<u64>,
        graphics: Option<&PluginGraphicsBridge>,
        event_proxy: Option<EventLoopProxy<ClientEvent>>,
    ) -> Result<Self> {
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
            outbound_session_messages: VecDeque::new(),
            outbound_session_commands: VecDeque::new(),
            outbound_host_messages: VecDeque::new(),
            pending_pane_creations: VecDeque::new(),
            bound_session_id: session_id,
            next_actor_pane: u64::MAX,
        };
        if let Some(path) = init_actor {
            runtime.load_init_actor(path, session_id, graphics)?;
        }
        for spec in specs {
            let pane = runtime.allocate_actor_pane()?;
            runtime.load_actor(spec, pane, graphics)?;
        }
        Ok(runtime)
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
            self.graphics.clone().as_ref(),
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
        session_id: Option<u64>,
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
        ))?;
        self.executor.block_on(actor.lifecycle(
            PluginLifecycleEvent::Start(PluginStartContext {
                instance_id: CLIENT_INIT_ACTOR_ID.to_string(),
                generation: actor.generation(),
                reason: PluginStartReason::Initial,
                session_id,
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
            PresentationActor { pane: 0, actor },
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
        graphics: Option<&PluginGraphicsBridge>,
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
            artifact => self.load_hosted_actor(spec, pane, graphics, artifact),
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
        graphics: Option<&PluginGraphicsBridge>,
        artifact: WasmPluginArtifact,
    ) -> Result<()> {
        let watcher = self.filesystem_watcher_service.clone();
        let presentation = graphics.map(|graphics| graphics.actor_host(pane));
        let actor = self.executor.block_on(load_hosted_actor(
            &spec,
            watcher,
            presentation.clone(),
            artifact,
            false,
        ))?;
        if let Some(presentation) = presentation {
            presentation.bind_generation(actor.generation());
        }
        eprintln!(
            "started presentation WASM actor id={} kind={} generation={} path={}",
            spec.id,
            actor.kind(),
            actor.generation(),
            spec.path.display()
        );
        self.actors
            .insert(spec.id, PresentationActor { pane, actor });
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
                        id: actor_id,
                        message,
                    });
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
                } if actor_id == CLIENT_INIT_ACTOR_ID => {
                    self.handle_init_start(id, &path, &capabilities, graphics);
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
                        id: actor_id,
                        message,
                    });
                }
                ComponentActorEvent::DispatcherFailed { message } => {
                    anyhow::bail!("presentation actor {actor_id:?} dispatcher failed: {message}")
                }
                ComponentActorEvent::WorkerFailed { worker, message } => anyhow::bail!(
                    "presentation actor {actor_id:?} worker {} failed: {message}",
                    worker.raw()
                ),
                ComponentActorEvent::Stopped if self.is_init_child(&actor_id) => {
                    self.send_init_child_event(ComponentChildEvent::Stopped { id: actor_id });
                }
                ComponentActorEvent::Stopped => {
                    anyhow::bail!("presentation actor {actor_id:?} stopped");
                }
                ComponentActorEvent::SessionMessageRequested {
                    session_id,
                    actor,
                    body,
                } => {
                    ensure!(
                        self.bound_session_id == Some(session_id),
                        "presentation actor addressed session {session_id} outside its bound viewport"
                    );
                    ensure!(
                        self.outbound_session_messages.len() < 64,
                        "presentation actor session-message queue is full"
                    );
                    self.outbound_session_messages
                        .push_back(SessionActorMessageRequest {
                            sender: actor_id,
                            session_id,
                            actor,
                            body,
                        });
                }
                ComponentActorEvent::LocalMessageRequested { actor, body } => {
                    self.send_actor_message(&actor, actor_id, body)?;
                }
                ComponentActorEvent::SessionCommandRequested(request) => {
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
    }

    fn handle_init_start(
        &mut self,
        id: String,
        guest_path: &str,
        capabilities: &[String],
        graphics: Option<&PluginGraphicsBridge>,
    ) {
        let result = self.start_init_child(&id, guest_path, capabilities, graphics);
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
        graphics: Option<&PluginGraphicsBridge>,
    ) -> Result<u64> {
        ensure!(
            capabilities.is_empty(),
            "presentation init children cannot request backend capabilities"
        );
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
        self.load_actor(spec, pane, graphics)?;
        let generation = self
            .actors
            .get(id)
            .context("init child did not produce a hosted actor")?
            .actor
            .generation();
        if let Some(init) = self.init.as_mut() {
            init.children.insert(id.to_string());
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
        let result = self.actors.remove(&id).map_or_else(
            || Err(anyhow::anyhow!("init child actor is unavailable")),
            |mut actor| self.executor.block_on(actor.actor.shutdown()),
        );
        if let Some(init) = self.init.as_mut() {
            init.children.remove(&id);
        }
        match result {
            Ok(()) => self.send_init_child_event(ComponentChildEvent::Stopped { id }),
            Err(error) => self.send_init_child_event(ComponentChildEvent::Failed {
                id,
                message: format!("{error:#}"),
            }),
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

    pub fn drain_session_messages(&mut self) -> Vec<SessionActorMessageRequest> {
        self.outbound_session_messages.drain(..).collect()
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

    pub fn is_empty(&self) -> bool {
        self.actors.is_empty() && self.legacy_actors.is_empty()
    }

    pub fn pane_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.actors
            .values()
            .filter(|actor| actor.pane != 0)
            .map(|actor| actor.pane)
    }

    pub fn drain_pending_pane_creations(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.pending_pane_creations.drain(..)
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

    pub fn actor_identities(&self) -> Vec<ActorIdentity> {
        self.actors
            .keys()
            .chain(self.legacy_actors.keys())
            .filter_map(|name| self.actor_identity(name))
            .collect()
    }

    pub const fn bound_session_id(&self) -> Option<u64> {
        self.bound_session_id
    }

    pub const fn bind_session(&mut self, session_id: u64) {
        self.bound_session_id = Some(session_id);
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
) -> Result<HostedWasmActor> {
    let root = spec.path.parent().unwrap_or_else(|| Path::new("."));
    let actor = match artifact {
        WasmPluginArtifact::Component { core_module_error } => {
            let actor = ComponentActor::load(
                &spec.path,
                ComponentActorConfig {
                    filesystem: Some(FilesystemGrant {
                        host_path: root.to_path_buf(),
                        guest_path: ".".to_string(),
                        writable: false,
                    }),
                    filesystem_watcher_service: watcher,
                    runtime_io: RuntimeIoConfig {
                        capabilities: RuntimeIoCapabilities::default(),
                        ..RuntimeIoConfig::default()
                    },
                    presentation: presentation.map(|presentation| {
                        presentation as std::sync::Arc<dyn brood_wasm::PresentationHost>
                    }),
                    actor_management,
                    session_messaging: true,
                    session_control: true,
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
                        host_path: root.to_path_buf(),
                        guest_path: ".".to_string(),
                        writable: false,
                    }),
                    runtime_io: RuntimeIoCapabilities::default(),
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

const fn presentation_event_pane(event: &PresentationEvent) -> u64 {
    match event {
        PresentationEvent::PaneCreated { pane, .. }
        | PresentationEvent::PaneResized { pane, .. }
        | PresentationEvent::PaneFocused { pane, .. }
        | PresentationEvent::PointerMoved { pane, .. }
        | PresentationEvent::PointerButton { pane, .. }
        | PresentationEvent::PointerScroll { pane, .. }
        | PresentationEvent::Key { pane, .. }
        | PresentationEvent::Text { pane, .. } => *pane,
    }
}
