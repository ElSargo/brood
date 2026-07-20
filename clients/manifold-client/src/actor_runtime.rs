use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use manifold_wasm::{
    ComponentActor, ComponentActorConfig, ComponentActorEvent, FilesystemGrant, HostedWasmActor,
    NativeFilesystemWatcherService, Preview1ActorCapabilities, Preview1CompatActor,
    Preview1FilesystemGrant, Preview1WorkerConfig, RuntimeIoCapabilities, RuntimeIoConfig,
    WasmPluginArtifact,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct PresentationActorSpec {
    id: String,
    path: PathBuf,
}

pub struct PresentationWasmRuntime {
    executor: tokio::runtime::Runtime,
    actors: BTreeMap<String, HostedWasmActor>,
    filesystem_watcher_service: NativeFilesystemWatcherService,
}

impl PresentationWasmRuntime {
    pub fn start(actor_specs: &[String]) -> Result<Self> {
        let specs = actor_specs
            .iter()
            .map(|spec| PresentationActorSpec::parse(spec))
            .collect::<Result<Vec<_>>>()?;
        let executor = tokio::runtime::Builder::new_multi_thread()
            .thread_name("manifold-client-wasm")
            .enable_all()
            .build()
            .context("start presentation WASM executor")?;
        let mut runtime = Self {
            executor,
            actors: BTreeMap::new(),
            filesystem_watcher_service: NativeFilesystemWatcherService::new(),
        };
        for spec in specs {
            runtime.load_actor(spec)?;
        }
        Ok(runtime)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the loaded actor is intentionally transferred into the long-lived client runtime"
    )]
    fn load_actor(&mut self, spec: PresentationActorSpec) -> Result<()> {
        ensure!(
            !self.actors.contains_key(&spec.id),
            "duplicate presentation actor id {:?}",
            spec.id
        );
        let watcher = self.filesystem_watcher_service.clone();
        let actor = self.executor.block_on(load_actor(&spec, watcher))?;
        eprintln!(
            "started presentation WASM actor id={} kind={} generation={} path={}",
            spec.id,
            actor.kind(),
            actor.generation(),
            spec.path.display()
        );
        self.actors.insert(spec.id, actor);
        Ok(())
    }

    pub fn drain_events(&mut self) -> Result<()> {
        for (actor_id, actor) in &mut self.actors {
            match actor {
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

    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let actor_ids = self.actors.keys().cloned().collect::<Vec<_>>();
        let mut first_error = None;
        for actor_id in actor_ids {
            let Some(mut actor) = self.actors.remove(&actor_id) else {
                continue;
            };
            if let Err(error) = self.executor.block_on(actor.shutdown())
                && first_error.is_none()
            {
                first_error = Some(error);
            }
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

async fn load_actor(
    spec: &PresentationActorSpec,
    watcher: NativeFilesystemWatcherService,
) -> Result<HostedWasmActor> {
    let root = spec.path.parent().unwrap_or_else(|| Path::new("."));
    let artifact = WasmPluginArtifact::load(&spec.path)?;
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
            anyhow::bail!(
                "legacy core WASM modules are not presentation actors; use a Component or Preview 1 actor"
            );
        }
    };

    Ok(actor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actor_spec_keeps_explicit_identity_and_path() {
        let result = PresentationActorSpec::parse("layout=/tmp/layout.component.wasm");
        assert!(result.is_ok());
        if let Ok(spec) = result {
            assert_eq!(spec.id, "layout");
            assert_eq!(spec.path, Path::new("/tmp/layout.component.wasm"));
        }
    }

    #[test]
    fn actor_spec_rejects_missing_identity_or_path() {
        assert!(PresentationActorSpec::parse("/tmp/layout.component.wasm").is_err());
        assert!(PresentationActorSpec::parse("=/tmp/layout.component.wasm").is_err());
        assert!(PresentationActorSpec::parse("layout=").is_err());
    }
}
