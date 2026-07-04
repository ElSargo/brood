use anyhow::{Context, Result, ensure};
use wasmtime::{ExternType, Module};

use crate::{
    api_link::EPOCH_RUN_EXPORT,
    module::{DEFAULT_ACTOR_NAME, WasmActorDeclaration, WasmModuleInventory},
    policy_provider_link::{POLICY_PROVIDER_EXPORT_PREFIX, parse_policy_provider_export},
};

pub const ACTOR_EXPORT_PREFIX: &str = "manifold_actor_";
pub const ACTOR_EXPORT_SUFFIX: &str = "_epoch_run";

pub fn discover_module_inventory(module: &Module) -> Result<WasmModuleInventory> {
    let mut actors = Vec::new();
    let mut policy_providers = Vec::new();

    for export in module.exports() {
        if !matches!(export.ty(), ExternType::Func(_)) {
            continue;
        }
        let name = export.name();
        if is_actor_export_name(name) {
            let actor = parse_actor_export(name)
                .with_context(|| format!("invalid actor export {name:?}"))?;
            actors.push(actor);
            continue;
        }
        if name.starts_with(POLICY_PROVIDER_EXPORT_PREFIX) {
            let provider = parse_policy_provider_export(name)
                .with_context(|| format!("invalid policy provider export {name:?}"))?;
            policy_providers.push(provider);
        }
    }

    ensure_unique_names("actor", actors.iter().map(|actor| actor.name.as_str()))?;
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
    })
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
