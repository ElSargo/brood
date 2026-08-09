use std::path::Path;

use anyhow::{Context, Result, ensure};
use brood_wasm::{
    PointerPolicyInvocation, PointerPolicyQuery, PointerPolicyResult, PolicyProviderKind,
    WasmPluginArtifact, WasmPointerPolicy,
};

/// The presentation-local synchronous pointer policy selected alongside the
/// configured workspace key policy.
///
/// A module may provide key policy without providing pointer policy. That is a
/// supported compatibility case: ordinary pane input continues to work and
/// layout interaction zones remain inert until a pointer provider is present.
pub struct ClientPointerPolicy {
    wasm: Option<WasmPointerPolicy>,
}

impl ClientPointerPolicy {
    pub fn load(spec: Option<&[String]>) -> Result<Self> {
        let Some(spec) = spec else {
            return Ok(Self { wasm: None });
        };
        ensure!(
            spec.len() == 2,
            "pointer policy requires a provider name and WASM path"
        );
        let provider = spec.first().context("pointer policy provider is missing")?;
        let path = spec.get(1).context("pointer policy path is missing")?;
        let artifact = WasmPluginArtifact::load(Path::new(path))
            .with_context(|| format!("inspect pointer policy module {path}"))?;
        let declares_pointer = matches!(
            artifact,
            WasmPluginArtifact::Legacy(ref legacy)
                if legacy.inventory.policy_providers.iter().any(|candidate| {
                    candidate.kind == PolicyProviderKind::Pointer && candidate.name == *provider
                })
        );
        if !declares_pointer {
            eprintln!(
                "presentation key policy module has no matching pointer provider; provider={provider} path={path}"
            );
            return Ok(Self { wasm: None });
        }
        let wasm = WasmPointerPolicy::load(Path::new(path), provider).with_context(|| {
            format!("load presentation pointer policy {provider:?} from {path}")
        })?;
        eprintln!("loaded presentation pointer policy provider={provider} path={path}");
        Ok(Self { wasm: Some(wasm) })
    }

    pub fn evaluate(&mut self, query: PointerPolicyQuery) -> Result<PointerPolicyResult> {
        let Some(wasm) = self.wasm.as_mut() else {
            return Ok(PointerPolicyResult::default());
        };
        wasm.handle_pointer(PointerPolicyInvocation::new(query, None))
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.wasm.is_some()
    }
}
