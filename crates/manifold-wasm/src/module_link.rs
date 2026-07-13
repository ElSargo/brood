use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use wasmtime::{ExternType, Module};

use crate::{
    api_link::EPOCH_RUN_EXPORT,
    module::{DEFAULT_ACTOR_NAME, WasmActorDeclaration, WasmActorEventKind, WasmModuleInventory},
    policy_provider_link::{POLICY_PROVIDER_EXPORT_PREFIX, parse_policy_provider_export},
};

pub const ACTOR_EXPORT_PREFIX: &str = "manifold_actor_";
pub const ACTOR_EXPORT_SUFFIX: &str = "_epoch_run";
pub const ACTOR_SUBSCRIPTION_INFIX: &str = "_subscribe_";

pub fn discover_module_inventory(module: &Module) -> Result<WasmModuleInventory> {
    let mut actors = Vec::new();
    let mut policy_providers = Vec::new();

    let mut subscription_exports = Vec::new();
    for export in module.exports() {
        let name = export.name();
        if is_actor_export_name(name) {
            ensure!(
                matches!(export.ty(), ExternType::Func(_)),
                "actor export {name:?} must be a function"
            );
            let actor = parse_actor_export(name)
                .with_context(|| format!("invalid actor export {name:?}"))?;
            actors.push(actor);
            continue;
        }
        if name.starts_with(ACTOR_EXPORT_PREFIX) && name.contains(ACTOR_SUBSCRIPTION_INFIX) {
            let subscription = parse_actor_subscription_export(name)
                .with_context(|| format!("invalid actor subscription export {name:?}"))?;
            let ExternType::Func(function_type) = export.ty() else {
                anyhow::bail!("actor subscription export {name:?} must be a function");
            };
            ensure!(
                function_type.params().len() == 0 && function_type.results().len() == 0,
                "actor subscription export {name:?} must have type () -> ()"
            );
            subscription_exports.push(subscription);
            continue;
        }
        if !matches!(export.ty(), ExternType::Func(_)) {
            continue;
        }
        if name.starts_with(POLICY_PROVIDER_EXPORT_PREFIX) {
            let provider = parse_policy_provider_export(name)
                .with_context(|| format!("invalid policy provider export {name:?}"))?;
            policy_providers.push(provider);
        }
    }

    ensure_unique_names("actor", actors.iter().map(|actor| actor.name.as_str()))?;
    for (actor_name, event) in subscription_exports {
        let actor = actors
            .iter_mut()
            .find(|actor| actor.name == actor_name)
            .with_context(|| {
                format!(
                    "event subscription for unknown actor {actor_name:?} ({})",
                    event.message_type()
                )
            })?;
        ensure!(
            actor.subscriptions.insert(event),
            "duplicate {:?} subscription for actor {actor_name:?}",
            event.message_type()
        );
    }
    ensure_unique_names(
        "policy provider export",
        policy_providers
            .iter()
            .map(|provider| provider.export_name.as_str()),
    )?;

    actors.sort_by(|left, right| left.name.cmp(&right.name));
    policy_providers.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.export_name.cmp(&right.export_name))
    });

    Ok(WasmModuleInventory {
        actors,
        policy_providers,
    })
}

pub fn parse_actor_export(export_name: &str) -> Option<WasmActorDeclaration> {
    if export_name == EPOCH_RUN_EXPORT {
        return Some(WasmActorDeclaration {
            name: DEFAULT_ACTOR_NAME.to_string(),
            export_name: export_name.to_string(),
            subscriptions: BTreeSet::default(),
        });
    }

    let name = export_name
        .strip_prefix(ACTOR_EXPORT_PREFIX)?
        .strip_suffix(ACTOR_EXPORT_SUFFIX)?;
    if !crate::policy_provider::is_valid_declaration_name(name) {
        return None;
    }

    Some(WasmActorDeclaration {
        name: name.to_string(),
        export_name: export_name.to_string(),
        subscriptions: BTreeSet::default(),
    })
}

pub fn parse_actor_subscription_export(export_name: &str) -> Option<(String, WasmActorEventKind)> {
    let declaration = export_name.strip_prefix(ACTOR_EXPORT_PREFIX)?;
    let (actor_name, event_name) = declaration.rsplit_once(ACTOR_SUBSCRIPTION_INFIX)?;
    if !crate::policy_provider::is_valid_declaration_name(actor_name) {
        return None;
    }
    let event = match event_name {
        "pane_mapped" => WasmActorEventKind::PaneMapped,
        "layout_state" => WasmActorEventKind::LayoutState,
        _ => return None,
    };
    Some((actor_name.to_string(), event))
}

pub fn is_actor_export_name(export_name: &str) -> bool {
    export_name == EPOCH_RUN_EXPORT
        || (export_name.starts_with(ACTOR_EXPORT_PREFIX)
            && export_name.ends_with(ACTOR_EXPORT_SUFFIX))
}

fn ensure_unique_names<'a>(label: &str, names: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        ensure!(seen.insert(name.to_string()), "duplicate {label} {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(exports: &[&str]) -> Result<WasmModuleInventory> {
        let exports = exports
            .iter()
            .map(|name| format!(r#"(func (export "{name}"))"#))
            .collect::<Vec<_>>()
            .join("\n");
        let module = Module::new(&wasmtime::Engine::default(), format!("(module {exports})"))
            .map_err(|error| anyhow::anyhow!("compile test module: {error}"))?;
        discover_module_inventory(&module)
    }

    #[test]
    fn valid_subscription_marker_is_attached_to_declared_actor() -> Result<()> {
        let inventory = inventory(&[
            "manifold_actor_layout_epoch_run",
            "manifold_actor_layout_subscribe_pane_mapped",
        ])?;
        let actor = inventory.actors.first().context("missing layout actor")?;
        ensure!(actor.subscribes_to(WasmActorEventKind::PaneMapped));
        ensure!(!actor.subscribes_to(WasmActorEventKind::LayoutState));
        Ok(())
    }

    #[test]
    fn subscription_for_unknown_actor_is_rejected() {
        assert!(inventory(&["manifold_actor_ghost_subscribe_layout_state"]).is_err());
    }

    #[test]
    fn unknown_subscription_event_is_rejected() {
        assert!(
            inventory(&[
                "manifold_actor_layout_epoch_run",
                "manifold_actor_layout_subscribe_typo",
            ])
            .is_err()
        );
    }

    #[test]
    fn subscription_marker_with_runtime_signature_is_rejected() -> Result<()> {
        let module = Module::new(
            &wasmtime::Engine::default(),
            r#"(module
                (func (export "manifold_actor_layout_epoch_run"))
                (func (export "manifold_actor_layout_subscribe_layout_state") (result i32)
                    i32.const 1))"#,
        )
        .map_err(|error| anyhow::anyhow!("compile signature fixture: {error}"))?;
        ensure!(discover_module_inventory(&module).is_err());
        Ok(())
    }

    #[test]
    fn non_function_subscription_marker_is_rejected() -> Result<()> {
        let module = Module::new(
            &wasmtime::Engine::default(),
            r#"(module
                (func (export "manifold_actor_layout_epoch_run"))
                (global (export "manifold_actor_layout_subscribe_layout_state") i32
                    (i32.const 1)))"#,
        )
        .map_err(|error| anyhow::anyhow!("compile non-function subscription fixture: {error}"))?;
        let Err(error) = discover_module_inventory(&module) else {
            anyhow::bail!("non-function subscription marker was accepted");
        };
        ensure!(
            error.to_string()
                == "actor subscription export \"manifold_actor_layout_subscribe_layout_state\" must be a function"
        );
        Ok(())
    }

    #[test]
    fn non_function_actor_export_is_rejected() -> Result<()> {
        let module = Module::new(
            &wasmtime::Engine::default(),
            r#"(module
                (global (export "manifold_actor_layout_epoch_run") i32
                    (i32.const 1)))"#,
        )
        .map_err(|error| anyhow::anyhow!("compile non-function actor fixture: {error}"))?;
        let Err(error) = discover_module_inventory(&module) else {
            anyhow::bail!("non-function actor export was accepted");
        };
        ensure!(
            error.to_string()
                == "actor export \"manifold_actor_layout_epoch_run\" must be a function"
        );
        Ok(())
    }
}
