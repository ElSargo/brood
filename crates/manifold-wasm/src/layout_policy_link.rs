use anyhow::{Result, ensure};
use wasmtime::{AsContextMut, Instance, TypedFunc};

use crate::{
    layout_policy::WasmLayoutPolicyEntry,
    policy_provider::{PolicyProviderKind, WasmPolicyProvider},
};

pub const LAYOUT_POLICY_RESULT_OK: i32 = crate::POLICY_PROVIDER_RESPONSE_OK;
pub const LAYOUT_POLICY_RESULT_ABSTAIN: i32 = crate::POLICY_PROVIDER_RESPONSE_ABSTAIN;
pub const LAYOUT_POLICY_RESULT_ERROR: i32 = crate::POLICY_PROVIDER_RESPONSE_ERROR;

/// ABI for a layout policy provider export:
///
/// ```text
/// manifold_policy_provider_layout_<name>(
///   query_ptr: i32,
///   query_len: i32,
///   state_ptr: i32,
///   state_len: i32,
/// ) -> i32
/// ```
///
/// The host copies a UTF-8 JSON layout query and the latest publication frame,
/// when present, into guest memory before invocation. The provider returns a
/// pointer to a `PolicyProviderResponse` descriptor. The host copies the
/// descriptor and payload while guest execution is suspended, then validates the
/// resolved layout. Descriptor status `0` means abstain; negative status means
/// provider error.
pub type LayoutPolicyExport = TypedFunc<(i32, i32, i32, i32), i32>;

pub fn load_layout_policy_entry<T: 'static>(
    mut store: impl AsContextMut<Data = T>,
    instance: &Instance,
    provider: &WasmPolicyProvider,
) -> Result<WasmLayoutPolicyEntry> {
    let declaration = provider.declaration();
    ensure!(
        declaration.kind == PolicyProviderKind::Layout,
        "policy provider {:?} is not a layout provider",
        declaration.name
    );
    let func = instance
        .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, &declaration.export_name)
        .map_err(|err| {
            anyhow::anyhow!(
                "load layout policy export {:?}: {err}",
                declaration.export_name
            )
        })?;
    Ok(WasmLayoutPolicyEntry::from_export(func))
}
