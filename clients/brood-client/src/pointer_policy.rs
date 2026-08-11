use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use brood_layout::{LayoutLeaseId, LayoutLeaseToken};
use brood_wasm::{
    PointerPolicyInvocation, PointerPolicyQuery, PointerPolicyResult, PolicyProviderKind,
    WasmPluginArtifact, WasmPointerPolicy,
};

use crate::actor_runtime::ActorIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointerPolicyBindingKey {
    pub lease: LayoutLeaseToken,
    pub holder_name: String,
    pub holder_generation: u64,
    pub provider: String,
}

struct PointerPolicyBinding {
    key: PointerPolicyBindingKey,
    wasm: WasmPointerPolicy,
}

/// Presentation-local pointer-provider catalogue and lease-owned instances.
///
/// The catalogue contains immutable provider declarations. Each accepted
/// actor selection creates a new Wasmtime store which lives only for the full
/// lease-token and actor-generation binding.
pub struct ClientPointerPolicy {
    catalogue: BTreeMap<String, PathBuf>,
    bindings: BTreeMap<LayoutLeaseId, PointerPolicyBinding>,
}

impl ClientPointerPolicy {
    pub fn load(spec: Option<&[String]>) -> Result<Self> {
        let mut policy = Self {
            catalogue: BTreeMap::new(),
            bindings: BTreeMap::new(),
        };
        let Some(spec) = spec else {
            return Ok(policy);
        };
        ensure!(
            spec.len() == 2,
            "pointer policy catalogue requires a configured provider name and WASM path"
        );
        let path = spec
            .get(1)
            .context("pointer policy catalogue path is missing")?;
        policy.register_module(Path::new(path))?;
        Ok(policy)
    }

    fn register_module(&mut self, path: &Path) -> Result<()> {
        let artifact = WasmPluginArtifact::load(path)
            .with_context(|| format!("inspect pointer policy module {}", path.display()))?;
        let WasmPluginArtifact::Legacy(legacy) = artifact else {
            return Ok(());
        };
        for provider in legacy
            .inventory
            .policy_providers
            .iter()
            .filter(|provider| provider.kind == PolicyProviderKind::Pointer)
        {
            if let Some(existing) = self.catalogue.get(&provider.name) {
                ensure!(
                    existing == path,
                    "duplicate pointer policy provider {:?} in {} and {}",
                    provider.name,
                    existing.display(),
                    path.display()
                );
                continue;
            }
            self.catalogue
                .insert(provider.name.clone(), path.to_path_buf());
            eprintln!(
                "registered pointer-policy provider={} path={}",
                provider.name,
                path.display()
            );
        }
        Ok(())
    }

    pub fn bind(
        &mut self,
        lease: LayoutLeaseToken,
        holder: &ActorIdentity,
        provider: &str,
    ) -> Result<PointerPolicyBindingKey> {
        ensure!(
            lease.id.0 != 0 && lease.generation != 0,
            "pointer policy binding has an invalid lease token"
        );
        ensure!(
            !provider.is_empty(),
            "pointer policy provider name is empty"
        );
        let key = PointerPolicyBindingKey {
            lease,
            holder_name: holder.name.clone(),
            holder_generation: holder.generation,
            provider: provider.to_string(),
        };
        if self
            .bindings
            .get(&lease.id)
            .is_some_and(|binding| binding.key == key)
        {
            return Ok(key);
        }
        // Remove the admitted authority first. Load the fresh provider before
        // admitting its replacement, so a failed selection leaves the lease
        // inert instead of retaining stale authority.
        self.bindings.remove(&lease.id);
        let path = self
            .catalogue
            .get(provider)
            .cloned()
            .with_context(|| format!("unknown pointer policy provider {provider:?}"))?;
        let wasm = WasmPointerPolicy::load(&path, provider).with_context(|| {
            format!(
                "instantiate pointer policy provider {provider:?} for lease {} generation {} actor {:?} actor_generation {}",
                lease.id.0, lease.generation, holder.name, holder.generation
            )
        })?;
        self.bindings.insert(
            lease.id,
            PointerPolicyBinding {
                key: key.clone(),
                wasm,
            },
        );
        Ok(key)
    }

    pub fn binding_key(
        &self,
        lease: LayoutLeaseToken,
        holder: &ActorIdentity,
    ) -> Option<PointerPolicyBindingKey> {
        self.bindings.get(&lease.id).and_then(|binding| {
            (binding.key.lease == lease
                && binding.key.holder_name == holder.name
                && binding.key.holder_generation == holder.generation)
                .then(|| binding.key.clone())
        })
    }

    pub fn contains_binding(&self, key: &PointerPolicyBindingKey) -> bool {
        self.bindings
            .get(&key.lease.id)
            .is_some_and(|binding| &binding.key == key)
    }

    pub fn evaluate(
        &mut self,
        key: &PointerPolicyBindingKey,
        query: PointerPolicyQuery,
    ) -> Result<PointerPolicyResult> {
        let binding = self
            .bindings
            .get_mut(&key.lease.id)
            .filter(|binding| &binding.key == key)
            .context("pointer policy binding is unavailable or stale")?;
        binding
            .wasm
            .handle_pointer(PointerPolicyInvocation::new(query, None))
    }

    pub fn revoke(&mut self, key: &PointerPolicyBindingKey) {
        if self
            .bindings
            .get(&key.lease.id)
            .is_some_and(|binding| &binding.key == key)
        {
            self.bindings.remove(&key.lease.id);
        }
    }

    pub fn revoke_all(&mut self) {
        self.bindings.clear();
    }

    pub fn retain_current(
        &mut self,
        leases: &[LayoutLeaseToken],
        holders: &BTreeMap<LayoutLeaseId, ActorIdentity>,
    ) {
        self.bindings.retain(|lease_id, binding| {
            leases.contains(&binding.key.lease)
                && holders.get(lease_id).is_some_and(|holder| {
                    holder.name == binding.key.holder_name
                        && holder.generation == binding.key.holder_generation
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> (tempfile::TempDir, ClientPointerPolicy) {
        let directory = tempfile::tempdir().expect("create pointer-policy test directory");
        let path = directory.path().join("provider.wat");
        std::fs::write(
            &path,
            r#"(module
                (import "brood" "read_message" (func $read_message (result i32)))
                (memory (export "memory") 4 64)
                (global $next (mut i32) (i32.const 1024))
                (func (export "brood_arena_alloc") (param $capacity i32) (param $align i32) (result i32)
                    (local $result i32)
                    global.get $next
                    local.tee $result
                    local.get $capacity
                    i32.add
                    global.set $next
                    local.get $result)
                (func (export "brood_policy_provider_pointer_test")
                    (param i32 i32 i32 i32) (result i32)
                    i32.const 0))"#,
        )
        .expect("write pointer-policy test module");
        let mut policy = ClientPointerPolicy {
            catalogue: BTreeMap::new(),
            bindings: BTreeMap::new(),
        };
        policy
            .register_module(&path)
            .expect("register pointer-policy test module");
        (directory, policy)
    }

    #[test]
    fn binding_key_requires_full_lease_and_actor_generations() {
        let lease = LayoutLeaseToken {
            id: LayoutLeaseId(7),
            generation: 3,
        };
        let holder = ActorIdentity {
            name: "shell".to_string(),
            generation: 11,
        };
        let key = PointerPolicyBindingKey {
            lease,
            holder_name: holder.name.clone(),
            holder_generation: holder.generation,
            provider: "divider".to_string(),
        };
        assert_eq!(key.lease, lease);
        assert_eq!(key.holder_generation, 11);
        assert_ne!(
            key,
            PointerPolicyBindingKey {
                lease: LayoutLeaseToken {
                    generation: 4,
                    ..lease
                },
                ..key.clone()
            }
        );
        assert_ne!(
            key,
            PointerPolicyBindingKey {
                holder_generation: 12,
                ..key.clone()
            }
        );
    }

    #[test]
    fn unknown_provider_cannot_create_a_binding() {
        let mut policy = ClientPointerPolicy {
            catalogue: BTreeMap::new(),
            bindings: BTreeMap::new(),
        };
        let error = policy
            .bind(
                LayoutLeaseToken {
                    id: LayoutLeaseId(1),
                    generation: 1,
                },
                &ActorIdentity {
                    name: "shell".to_string(),
                    generation: 1,
                },
                "missing",
            )
            .expect_err("unknown provider must fail");
        assert!(
            error
                .to_string()
                .contains("unknown pointer policy provider")
        );
        assert!(policy.bindings.is_empty());
    }

    #[test]
    fn failed_reselection_leaves_the_lease_inert() {
        let (_directory, mut policy) = test_policy();
        let lease = LayoutLeaseToken {
            id: LayoutLeaseId(1),
            generation: 2,
        };
        let holder = ActorIdentity {
            name: "shell".to_string(),
            generation: 3,
        };
        policy.bind(lease, &holder, "test").expect("bind provider");
        assert!(policy.binding_key(lease, &holder).is_some());
        assert!(policy.bind(lease, &holder, "missing").is_err());
        assert!(policy.binding_key(lease, &holder).is_none());
    }

    #[test]
    fn retention_revokes_a_stale_actor_generation() {
        let (_directory, mut policy) = test_policy();
        let lease = LayoutLeaseToken {
            id: LayoutLeaseId(4),
            generation: 5,
        };
        let holder = ActorIdentity {
            name: "workspace".to_string(),
            generation: 6,
        };
        policy.bind(lease, &holder, "test").expect("bind provider");
        policy.retain_current(
            &[lease],
            &BTreeMap::from([(
                lease.id,
                ActorIdentity {
                    generation: 7,
                    ..holder.clone()
                },
            )]),
        );
        assert!(policy.binding_key(lease, &holder).is_none());
    }
}
