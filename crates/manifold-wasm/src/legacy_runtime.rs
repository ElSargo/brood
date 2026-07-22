use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{Mutex, watch};
use tracing::{info, warn};

use crate::{
    ActorEpochOutcome, ActorId, Registry, WasmActor, WasmActorDeclaration, WasmActorEventKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyWasmRuntimeEvent {
    DrainHostMailbox,
}

/// Owns all tasks and cancellation state for one legacy core-WASM generation.
pub struct LegacyWasmActorGeneration {
    declarations: Vec<WasmActorDeclaration>,
    actor_ids: BTreeMap<String, ActorId>,
    cancellation: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl LegacyWasmActorGeneration {
    pub fn start(
        runtime: &tokio::runtime::Handle,
        candidates: Vec<(WasmActorDeclaration, WasmActor)>,
        actor_ids: BTreeMap<String, ActorId>,
        registry: Arc<Mutex<Registry>>,
        path: &Path,
        event_handler: impl Fn(LegacyWasmRuntimeEvent) + Send + Sync + 'static,
    ) -> Self {
        let declarations = candidates
            .iter()
            .map(|(declaration, _actor)| declaration.clone())
            .collect();
        let event_handler: Arc<dyn Fn(LegacyWasmRuntimeEvent) + Send + Sync> =
            Arc::new(event_handler);
        let (cancellation, _) = watch::channel(false);
        let tasks = candidates
            .into_iter()
            .map(|(declaration, actor)| {
                spawn_actor_task(
                    runtime,
                    actor,
                    declaration,
                    Arc::clone(&registry),
                    Arc::clone(&event_handler),
                    cancellation.subscribe(),
                    path.to_path_buf(),
                )
            })
            .collect();
        Self {
            declarations,
            actor_ids,
            cancellation,
            tasks,
        }
    }

    pub fn actor_names(&self) -> Vec<String> {
        self.declarations
            .iter()
            .map(|actor| actor.name.clone())
            .collect()
    }

    pub const fn actor_ids(&self) -> &BTreeMap<String, ActorId> {
        &self.actor_ids
    }

    pub fn contains_actor(&self, actor_name: &str) -> bool {
        self.declarations
            .iter()
            .any(|actor| actor.name == actor_name)
    }

    pub fn subscribers(&self, event: WasmActorEventKind) -> Vec<String> {
        self.declarations
            .iter()
            .filter(|actor| actor.subscribes_to(event))
            .map(|actor| actor.name.clone())
            .collect()
    }

    pub async fn cancel_and_join(self) {
        let actor_names = self.actor_names();
        if self.cancellation.send(true).is_err() {
            tracing::debug!("legacy WASM generation had no cancellation receivers");
        }
        for (actor_name, task) in actor_names.into_iter().zip(self.tasks) {
            match task.await {
                Ok(()) => {}
                Err(error) => {
                    warn!(actor = %actor_name, error = %error, "legacy WASM actor join failed");
                }
            }
        }
    }
}

fn spawn_actor_task(
    runtime: &tokio::runtime::Handle,
    mut actor: WasmActor,
    declaration: WasmActorDeclaration,
    registry: Arc<Mutex<Registry>>,
    event_handler: Arc<dyn Fn(LegacyWasmRuntimeEvent) + Send + Sync>,
    mut cancellation: watch::Receiver<bool>,
    path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        info!(actor = %declaration.name, export = %declaration.export_name, path = %path.display(), "started WASM plugin actor generation");
        loop {
            let outcome = tokio::select! {
                biased;
                changed = cancellation.changed() => {
                    if changed.is_err() || *cancellation.borrow() {
                        info!(actor = %declaration.name, "cancelled WASM plugin actor generation");
                        return;
                    }
                    continue;
                }
                outcome = actor.run_one_epoch() => outcome,
            };
            match outcome {
                Ok(ActorEpochOutcome::Continue { outbox }) => {
                    if let Some(outbox) = outbox {
                        let dispatch = {
                            let mut registry = registry.lock().await;
                            actor.dispatch_outbox(&mut registry, &outbox)
                        };
                        if let Err(error) = dispatch {
                            warn!(actor = %declaration.name, error = %error, "WASM plugin actor outbox dispatch failed");
                        }
                        event_handler(LegacyWasmRuntimeEvent::DrainHostMailbox);
                    }
                }
                Ok(ActorEpochOutcome::Stop) => {
                    info!(actor = %declaration.name, "WASM plugin actor stopped");
                    return;
                }
                Err(error) => {
                    warn!(actor = %declaration.name, error = %error, "WASM plugin actor epoch failed");
                    return;
                }
            }
        }
    })
}
