use anyhow::{Context, Result, ensure};
use brood_protocol::facets::{InvocationLimits, RenderToken, Scope};
use brood_wasm::presentation::{PresentationError, TextLayoutOptions, TextLayoutResult};
use brood_wasm::{
    facet_resources::{FacetResourceBackend, FacetResourceRegistry, FacetResourceRegistryConfig},
    facet_runtime::{FacetOperation, FacetRuntime, FacetRuntimeConfig},
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
};
use std::{sync::Arc, time::Duration};

#[derive(Debug)]
struct NoopBackend;
impl FacetResourceBackend for NoopBackend {
    fn create_image(
        &self,
        _pane: u64,
        _width: u32,
        _height: u32,
        _pixels: &[u8],
        _max_bytes: usize,
    ) -> Result<(u64, usize), PresentationError> {
        Ok((1, 4))
    }
    fn create_text(
        &self,
        _pane: u64,
        _text: &str,
        _options: TextLayoutOptions,
        _max_bytes: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError> {
        Err(PresentationError::Unavailable)
    }
    fn release(&self, _pane: u64, _handle: u64) -> Result<(), PresentationError> {
        Ok(())
    }
}
use std::fs;

fn budget() -> ByteBudget {
    ByteBudget::new(
        1,
        ByteBudgetLimits {
            max_live_bytes: 4 * 1024 * 1024,
            max_live_allocations: 2,
            max_buffer_bytes: 4 * 1024 * 1024,
        },
    )
}

fn write_component(text: &str) -> Result<tempfile::NamedTempFile> {
    let file = tempfile::NamedTempFile::new()?;
    fs::write(file.path(), text)?;
    Ok(file)
}

fn facets_component(body: &str) -> String {
    format!(
        r#"(component
      (core module $m
        (memory (export "memory") 1)
        (func (export "render") (param $ptr i32) (param $len i32) (result i32)
          {body})
        (func (export "interpret") (param $ptr i32) (param $len i32) (result i32)
          i32.const 0 i32.const 0 i32.store i32.const 4 i32.const 0 i32.store i32.const 0)
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
      (core instance $i (instantiate $m))
      (alias core export $i "memory" (core memory $mem))
      (alias core export $i "render" (core func $render))
      (alias core export $i "interpret" (core func $interpret))
      (alias core export $i "realloc" (core func $realloc))
      (type $bytes-fn (func (param "input" (list u8)) (result (list u8))))
      (func $render-f (type $bytes-fn) (canon lift (core func $render) (memory $mem) (realloc $realloc)))
      (func $interpret-f (type $bytes-fn) (canon lift (core func $interpret) (memory $mem) (realloc $realloc)))
      (instance $facets
        (export "render" (func $render-f))
        (export "interpret" (func $interpret-f)))
      (export "brood:plugin/facets@0.1.0" (instance $facets)))"#
    )
}

#[test]
fn invokes_both_operations_and_missing_interface_is_optional() -> Result<()> {
    let component = write_component(&facets_component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0",
    ))?;
    let config = FacetRuntimeConfig::default();
    let mut runtime = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    let budget = budget();
    let output = runtime.invoke(FacetOperation::Render, b"abc", &budget)?;
    ensure!(output.bytes()? == b"abc");
    drop(output);
    let output = runtime.invoke(FacetOperation::Interpret, b"abc", &budget)?;
    ensure!(output.bytes()?.is_empty());

    let missing = write_component("(component)")?;
    ensure!(FacetRuntime::load(missing.path(), &config)?.is_none());
    Ok(())
}

#[test]
fn malformed_interface_is_rejected() -> Result<()> {
    let malformed = write_component(
        r#"(component (core module $m) (core instance $i (instantiate $m)) (export "brood:plugin/facets@0.1.0" (core instance $i)))"#,
    )?;
    ensure!(FacetRuntime::load(malformed.path(), &FacetRuntimeConfig::default()).is_err());
    Ok(())
}

#[test]
fn trap_invalidates_and_fresh_load_recovers() -> Result<()> {
    let component = write_component(&facets_component("unreachable"))?;
    let config = FacetRuntimeConfig::default();
    let mut runtime = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    ensure!(
        runtime
            .invoke(FacetOperation::Render, b"x", &budget())
            .is_err()
    );
    ensure!(!runtime.is_usable());
    ensure!(
        runtime
            .invoke(FacetOperation::Interpret, b"x", &budget())
            .is_err()
    );
    let fresh = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    ensure!(fresh.is_usable());
    Ok(())
}

#[test]
fn oversized_result_invalidates_runtime() -> Result<()> {
    let component = write_component(&facets_component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 i32.const 32 i32.store i32.const 0",
    ))?;
    let config = FacetRuntimeConfig {
        max_output_bytes: 16,
        ..FacetRuntimeConfig::default()
    };
    let mut runtime = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    ensure!(
        runtime
            .invoke(FacetOperation::Render, &[0; 32], &budget())
            .is_err()
    );
    ensure!(!runtime.is_usable());
    Ok(())
}

#[test]
fn budget_admission_rejection_preserves_runtime() -> Result<()> {
    let component = write_component(&facets_component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0",
    ))?;
    let config = FacetRuntimeConfig::default();
    let mut runtime = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    let too_small = ByteBudget::new(
        2,
        ByteBudgetLimits {
            max_live_bytes: 16,
            max_live_allocations: 1,
            max_buffer_bytes: 16,
        },
    );
    ensure!(
        runtime
            .invoke(FacetOperation::Render, b"abc", &too_small)
            .is_err()
    );
    ensure!(runtime.is_usable());
    Ok(())
}

#[test]
fn resource_invocation_seals_and_clears_lease() -> Result<()> {
    let component = write_component(&facets_component(
        "i32.const 0 local.get $ptr i32.store i32.const 4 local.get $len i32.store i32.const 0",
    ))?;
    let config = FacetRuntimeConfig::default();
    let mut runtime = FacetRuntime::load(component.path(), &config)?.context("missing facets")?;
    let registry = FacetResourceRegistry::new(
        Arc::new(NoopBackend),
        FacetResourceRegistryConfig {
            max_invocations: 1,
            retry_interval: Duration::from_millis(1),
        },
    )?;
    let resource_budget = budget();
    let lease = registry.begin(
        RenderToken {
            scope: Scope {
                client_generation: 1,
                actor_generation: 1,
                facet_generation: 1,
            },
            invocation: 1,
            snapshot_revision: 1,
            candidate: 1,
            pane_membership_revision: 1,
            geometry_revision: 1,
            host_configuration_revision: 1,
            renderer_generation: 1,
            interaction_revision: 1,
        },
        &[9],
        InvocationLimits {
            max_output_bytes: 64,
            max_packet_bytes: 64,
            max_interaction_bytes: 64,
            max_sequence_bytes: 64,
            max_events: 1,
            max_event_bytes: 64,
            max_host_requests: 1,
            max_resources: 1,
            max_resource_bytes: 64,
        },
        resource_budget,
    )?;
    let rejected_budget = ByteBudget::new(
        99,
        ByteBudgetLimits {
            max_live_bytes: 1,
            max_live_allocations: 1,
            max_buffer_bytes: 1,
        },
    );
    ensure!(
        runtime
            .invoke_with_resources(b"ok", &rejected_budget, Arc::clone(&lease))
            .is_err()
    );
    ensure!(runtime.is_usable());
    let temporary = lease.create_image(9, 1, 1, &[0, 0, 0, 0])?;
    lease.drop_resource(9, temporary)?;
    let output_budget = budget();
    let output = runtime.invoke_with_resources(b"ok", &output_budget, Arc::clone(&lease))?;
    ensure!(output.bytes()? == b"ok");
    ensure!(lease.create_image(9, 1, 1, &[0, 0, 0, 0]) == Err(PresentationError::InvalidToken));
    ensure!(runtime.is_usable());
    Ok(())
}
