use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use manifold_wasm::{
    ComponentActor, ComponentActorConfig, ComponentActorEvent, FilesystemGrant, HostedWasmActor,
    LegacyWasmActorGeneration, LegacyWasmRuntimeEvent, Mailbox, NativeFilesystemWatcherService,
    PresentationEvent, Preview1ActorCapabilities, Preview1CompatActor, Preview1FilesystemGrant,
    Preview1WorkerConfig, Registry, RuntimeIoCapabilities, RuntimeIoConfig, WasmActor,
    WasmActorDeclaration, WasmActorEventKind, WasmPluginArtifact,
};
use tokio::sync::Mutex;
use winit::event_loop::EventLoopProxy;

use crate::{ClientEvent, plugin_graphics::PluginGraphicsBridge};

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
        graphics: Option<&PluginGraphicsBridge>,
        event_proxy: Option<EventLoopProxy<ClientEvent>>,
    ) -> Result<Self> {
        let specs = actor_specs
            .iter()
            .map(|spec| PresentationActorSpec::parse(spec))
            .collect::<Result<Vec<_>>>()?;
        let executor = tokio::runtime::Builder::new_multi_thread()
            .thread_name("manifold-client-wasm")
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
        };
        for (index, spec) in specs.into_iter().enumerate() {
            // Backend-managed pane IDs grow upward from one. Client-local pane
            // actors grow downward from the top of the same flat namespace so
            // an actor-submitted layout can address both without knowing their
            // origin. Controller actors do not receive a pane identity.
            let pane = u64::MAX.saturating_sub(u64::try_from(index).unwrap_or(u64::MAX));
            ensure!(pane != 0, "presentation actor pane ID space is exhausted");
            runtime.load_actor(spec, pane, graphics)?;
        }
        Ok(runtime)
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
        for (actor_id, hosted) in &mut self.actors {
            match &mut hosted.actor {
                HostedWasmActor::Component(actor) => {
                    while let Some(event) = actor.try_recv_event() {
                        match event {
                            ComponentActorEvent::DispatcherFailed { message } => {
                                anyhow::bail!(
                                    "presentation actor {actor_id:?} dispatcher failed: {message}"
                                );
                            }
                            ComponentActorEvent::WorkerFailed { worker, message } => {
                                anyhow::bail!(
                                    "presentation actor {actor_id:?} worker {} failed: {message}",
                                    worker.raw()
                                );
                            }
                            ComponentActorEvent::Stopped => {
                                anyhow::bail!("presentation actor {actor_id:?} stopped");
                            }
                            _ => {}
                        }
                    }
                }
                HostedWasmActor::Preview1(actor) => {
                    if let Some(message) = actor.try_take_failure() {
                        anyhow::bail!(
                            "presentation actor {actor_id:?} dispatcher failed: {message}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn drain_host_messages(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut messages = Vec::new();
        while let Some(message) = self.host_mailbox.try_read_message()? {
            messages.push(message);
        }
        Ok(messages)
    }

    pub fn is_empty(&self) -> bool {
        self.actors.is_empty() && self.legacy_actors.is_empty()
    }

    pub fn pane_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.actors.values().map(|actor| actor.pane)
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

    pub fn send_actor_message(&self, actor_id: &str, payload: Vec<u8>) -> Result<()> {
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

    pub fn send_layout_state(&self, payload: &[u8]) -> Result<usize> {
        let recipients = self.layout_actor_ids();
        for actor_id in &recipients {
            self.send_legacy_message(actor_id, payload.to_owned())?;
        }
        Ok(recipients.len())
    }

    fn layout_actor_ids(&self) -> Vec<String> {
        self.legacy_actors
            .iter()
            .filter(|(_, actor)| {
                actor
                    .declaration
                    .subscribes_to(WasmActorEventKind::LayoutState)
            })
            .map(|(actor_id, _)| actor_id.clone())
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
                        presentation as std::sync::Arc<dyn manifold_wasm::PresentationHost>
                    }),
                    ..ComponentActorConfig::default()
                },
            )
            .await
            .with_context(|| {
                format!(
                    "{} is neither a core Manifold module ({core_module_error}) nor a component actor",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_spec_keeps_explicit_identity_and_path() {
        let result = PresentationActorSpec::parse("workspace=/tmp/workspace.wasm");
        assert!(result.is_ok());
        if let Ok(spec) = result {
            assert_eq!(spec.id, "workspace");
            assert_eq!(spec.path, Path::new("/tmp/workspace.wasm"));
        }
    }

    #[test]
    fn actor_spec_rejects_missing_identity_or_path() {
        assert!(PresentationActorSpec::parse("/tmp/workspace.wasm").is_err());
        assert!(PresentationActorSpec::parse("=/tmp/workspace.wasm").is_err());
        assert!(PresentationActorSpec::parse("workspace=").is_err());
    }
}
