use std::{collections::BTreeSet, path::Path};

use anyhow::{Context, Result, ensure};
use wasmtime::{Engine, Module};

use crate::{
    actor::{ActorEpochOutcome, WasmActor, WasmActorConfig},
    api::{Registry, async_epoch_engine, load_module_async},
    module_link::{discover_module_inventory, parse_actor_export},
    policy_provider::{PolicyProviderDeclaration, WasmPolicyProvider},
};

pub const DEFAULT_ACTOR_NAME: &str = "default";
pub const DEFAULT_ACTOR_MAILBOX_CAPACITY: usize = 64;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmActorDeclaration {
    pub name: String,
    pub export_name: String,
    pub subscriptions: BTreeSet<WasmActorEventKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WasmActorEventKind {
    PaneMapped,
    LayoutState,
}

impl WasmActorEventKind {
    pub const fn message_type(self) -> &'static str {
        match self {
            Self::PaneMapped => "pane-mapped",
            Self::LayoutState => "layout-state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WasmModuleInventory {
    pub actors: Vec<WasmActorDeclaration>,
    pub policy_providers: Vec<PolicyProviderDeclaration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WasmPluginConfig {
    pub actor: WasmActorConfig,
    pub actor_mailbox_capacity: usize,
}

pub struct WasmPluginModule {
    inventory: WasmModuleInventory,
    actors: Vec<WasmActor>,
    policy_providers: Vec<WasmPolicyProvider>,
}

impl Default for WasmPluginConfig {
    fn default() -> Self {
        Self {
            actor: WasmActorConfig::default(),
            actor_mailbox_capacity: DEFAULT_ACTOR_MAILBOX_CAPACITY,
        }
    }
}

impl WasmActorDeclaration {
    pub fn from_export_name(export_name: &str) -> Option<Self> {
        parse_actor_export(export_name)
    }

    pub fn subscribes_to(&self, event: WasmActorEventKind) -> bool {
        self.subscriptions.contains(&event)
    }
}

impl WasmModuleInventory {
    pub fn discover(module: &Module) -> Result<Self> {
        discover_module_inventory(module)
    }

    pub const fn is_empty(&self) -> bool {
        self.actors.is_empty() && self.policy_providers.is_empty()
    }
}

impl WasmPluginModule {
    pub async fn load(path: &Path, registry: &mut Registry) -> Result<Self> {
        Self::load_with_config(path, registry, WasmPluginConfig::default()).await
    }

    pub async fn load_with_config(
        path: &Path,
        registry: &mut Registry,
        config: WasmPluginConfig,
    ) -> Result<Self> {
        ensure!(
            config.actor_mailbox_capacity > 0,
            "actor mailbox capacity must be non-zero"
        );

        let engine = async_epoch_engine()?;
        let module = load_module_async(engine.clone(), path).await?;
        Self::load_from_module(engine, &module, registry, config).await
    }

    pub async fn load_from_module(
        engine: Engine,
        module: &Module,
        registry: &mut Registry,
        config: WasmPluginConfig,
    ) -> Result<Self> {
        let inventory = WasmModuleInventory::discover(module)?;
        ensure!(
            !inventory.is_empty(),
            "WASM module does not declare any actors or policy providers"
        );

        let mut actors = Vec::new();
        let mut registrations = Vec::new();
        for declaration in &inventory.actors {
            let (registration, mailbox) = registry
                .prepare_actor(declaration.name.clone(), config.actor_mailbox_capacity)
                .with_context(|| format!("prepare actor {:?}", declaration.name))?;
            let actor = WasmActor::load_from_module(
                engine.clone(),
                module,
                mailbox,
                config.actor,
                &declaration.export_name,
            )
            .await
            .with_context(|| format!("load actor {:?}", declaration.name))?;
            registrations.push(registration);
            actors.push(actor);
        }
        registry
            .activate_prepared_actors(registrations, &[])
            .context("activate plugin actor set")?;

        let policy_providers = inventory
            .policy_providers
            .iter()
            .cloned()
            .map(WasmPolicyProvider::new)
            .collect();

        Ok(Self {
            inventory,
            actors,
            policy_providers,
        })
    }

    pub const fn inventory(&self) -> &WasmModuleInventory {
        &self.inventory
    }

    pub fn actors(&self) -> &[WasmActor] {
        &self.actors
    }

    pub fn actors_mut(&mut self) -> &mut [WasmActor] {
        &mut self.actors
    }

    pub async fn run_actor_epoch(
        &mut self,
        actor_index: usize,
        registry: &mut Registry,
    ) -> Result<ActorEpochOutcome> {
        let actor = self
            .actors
            .get_mut(actor_index)
            .with_context(|| format!("unknown WASM actor index {actor_index}"))?;
        actor.run_one_epoch_and_dispatch(registry).await
    }

    pub async fn run_all_actor_epochs(
        &mut self,
        registry: &mut Registry,
    ) -> Result<Vec<ActorEpochOutcome>> {
        let mut statuses = Vec::with_capacity(self.actors.len());
        for actor in &mut self.actors {
            statuses.push(actor.run_one_epoch_and_dispatch(registry).await?);
        }
        Ok(statuses)
    }

    pub fn policy_providers(&self) -> &[WasmPolicyProvider] {
        &self.policy_providers
    }
}
