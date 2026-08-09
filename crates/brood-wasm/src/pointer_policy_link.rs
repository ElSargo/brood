use anyhow::{Result, ensure};
use wasmtime::{AsContextMut, Instance, TypedFunc};

use crate::{
    pointer_policy::WasmPointerPolicyEntry,
    policy_provider::{PolicyProviderKind, WasmPolicyProvider},
};

/// ABI for a pointer policy provider export:
///
/// ```text
/// brood_policy_provider_pointer_<name>(
///   query_ptr: i32,
///   query_len: i32,
///   state_ptr: i32,
///   state_len: i32,
/// ) -> i32
/// ```
///
/// Query, immutable provider state, and result payloads are bounded UTF-8 JSON.
/// The returned pointer names the same [`crate::PolicyProviderResponse`]
/// descriptor used by the key-policy lane.
pub type PointerPolicyExport = TypedFunc<(i32, i32, i32, i32), i32>;

pub fn load_pointer_policy_entry<T: 'static>(
    mut store: impl AsContextMut<Data = T>,
    instance: &Instance,
    provider: &WasmPolicyProvider,
) -> Result<WasmPointerPolicyEntry> {
    let declaration = provider.declaration();
    ensure!(
        declaration.kind == PolicyProviderKind::Pointer,
        "policy provider {:?} is not a pointer provider",
        declaration.name
    );
    let func = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, &declaration.export_name)
        .map_err(|error| {
            anyhow::anyhow!(
                "load pointer policy export {:?}: {error}",
                declaration.export_name
            )
        })?;
    Ok(WasmPointerPolicyEntry::from_export(func))
}
