use anyhow::{Result, ensure};
use wasmtime::{AsContextMut, Instance, TypedFunc};

use crate::{
    global_key_policy::WasmGlobalKeyPolicyEntry,
    policy_provider::{PolicyProviderKind, WasmPolicyProvider},
};

pub const GLOBAL_KEY_POLICY_RESULT_OK: i32 = crate::POLICY_PROVIDER_RESPONSE_OK;
pub const GLOBAL_KEY_POLICY_RESULT_ABSTAIN: i32 = crate::POLICY_PROVIDER_RESPONSE_ABSTAIN;
pub const GLOBAL_KEY_POLICY_RESULT_ERROR: i32 = crate::POLICY_PROVIDER_RESPONSE_ERROR;

/// ABI for a key policy provider export:
///
/// ```text
/// brood_policy_provider_key_<name>(
///   query_ptr: i32,
///   query_len: i32,
///   state_ptr: i32,
///   state_len: i32,
/// ) -> i32
/// ```
///
/// The host copies a UTF-8 JSON key query and the latest publication frame, when
/// present, into guest memory before invocation. The provider returns a pointer to
/// a `PolicyProviderResponse` descriptor. The host copies the descriptor and
/// payload while guest execution is suspended, then validates and applies the
/// command envelope. Descriptor status `0` means abstain; negative status means
/// provider error.
pub type GlobalKeyPolicyExport = TypedFunc<(i32, i32, i32, i32), i32>;

pub fn load_global_key_policy_entry<T: 'static>(
    mut store: impl AsContextMut<Data = T>,
    instance: &Instance,
    provider: &WasmPolicyProvider,
) -> Result<WasmGlobalKeyPolicyEntry> {
    let declaration = provider.declaration();
    ensure!(
        declaration.kind == PolicyProviderKind::Key,
        "policy provider {:?} is not a key provider",
        declaration.name
    );
    let func = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, &declaration.export_name)
        .map_err(|err| {
            anyhow::anyhow!(
                "load key policy export {:?}: {err}",
                declaration.export_name
            )
        })?;
    Ok(WasmGlobalKeyPolicyEntry::from_export(func))
}
