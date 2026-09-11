use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Result, ensure};
use brood_protocol::facets::{InvocationLimits, RenderToken, Scope};
use brood_wasm::{
    facet_resources::{FacetResourceBackend, FacetResourceRegistry, FacetResourceRegistryConfig},
    presentation::{PresentationError, TextLayoutOptions, TextLayoutResult},
    snapshot_budget::{ByteBudget, ByteBudgetLimits},
};

#[derive(Debug, Default)]
struct FakeBackend {
    resources: Mutex<BTreeMap<u64, usize>>,
    next: Mutex<u64>,
    blocked: Mutex<bool>,
    allocation_deferred: Mutex<bool>,
    released: Mutex<usize>,
    release_calls: Mutex<usize>,
    release_threads: Mutex<Vec<thread::ThreadId>>,
}

impl FakeBackend {
    fn set_blocked(&self, blocked: bool) {
        *self
            .blocked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = blocked;
    }
    fn released(&self) -> usize {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn release_calls(&self) -> usize {
        *self
            .release_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn release_threads(&self) -> Vec<thread::ThreadId> {
        self.release_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl FacetResourceBackend for FakeBackend {
    fn create_image(
        &self,
        _pane: u64,
        _width: u32,
        _height: u32,
        pixels: &[u8],
        max_bytes: usize,
    ) -> Result<(u64, usize), PresentationError> {
        if pixels.len() > max_bytes {
            return Err(PresentationError::LimitReached);
        }
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        let handle = *next;
        drop(next);
        self.resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle, pixels.len());
        Ok((handle, pixels.len()))
    }

    fn create_text(
        &self,
        _pane: u64,
        text: &str,
        _options: TextLayoutOptions,
        max_bytes: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError> {
        if *self
            .allocation_deferred
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(PresentationError::AdmissionDeferred);
        }
        if text.len() > max_bytes {
            return Err(PresentationError::LimitReached);
        }
        let mut next = self
            .next
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        let handle = *next;
        drop(next);
        self.resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(handle, text.len());
        Ok((
            handle,
            TextLayoutResult {
                handle,
                width: 1.0,
                height: 1.0,
                first_baseline: 1.0,
                line_count: 1,
                cell_width: 1.0,
                line_height: 1.0,
            },
            text.len(),
        ))
    }

    fn release(&self, _pane: u64, handle: u64) -> Result<(), PresentationError> {
        *self
            .release_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        self.release_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(thread::current().id());
        if *self
            .blocked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(PresentationError::ResourceInUse);
        }
        if self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&handle)
            .is_none()
        {
            return Err(PresentationError::InvalidResource);
        }
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Ok(())
    }
}

const fn token() -> RenderToken {
    RenderToken {
        scope: Scope {
            client_generation: 1,
            actor_generation: 2,
            facet_generation: 3,
        },
        invocation: 4,
        snapshot_revision: 5,
        candidate: 6,
        pane_membership_revision: 7,
        geometry_revision: 8,
        host_configuration_revision: 9,
        renderer_generation: 10,
        interaction_revision: 11,
    }
}
const fn limits() -> InvocationLimits {
    InvocationLimits {
        max_output_bytes: 64,
        max_packet_bytes: 64,
        max_interaction_bytes: 64,
        max_sequence_bytes: 64,
        max_events: 4,
        max_event_bytes: 64,
        max_host_requests: 4,
        max_resources: 2,
        max_resource_bytes: 4,
    }
}
fn registry(
    backend: Arc<FakeBackend>,
    max_invocations: usize,
) -> Result<FacetResourceRegistry, PresentationError> {
    FacetResourceRegistry::new(
        backend,
        FacetResourceRegistryConfig {
            max_invocations,
            retry_interval: Duration::from_millis(2),
        },
    )
}

fn budget() -> ByteBudget {
    ByteBudget::new(
        2,
        ByteBudgetLimits {
            max_live_bytes: 4096,
            max_live_allocations: 64,
            max_buffer_bytes: 4096,
        },
    )
}

#[test]
fn bounds_owner_validation_and_seal_are_enforced() -> Result<()> {
    let backend = Arc::new(FakeBackend::default());
    let registry = registry(Arc::clone(&backend), 2)?;
    let lease = registry.begin(token(), &[7], limits(), budget())?;
    let handle = lease.create_image(7, 1, 1, &[1, 2, 3, 4])?;
    ensure!(lease.create_image(7, 1, 1, &[1, 2, 3, 4]).is_err());
    ensure!(
        !lease.allocation_deferred(),
        "hard limits must not retry forever"
    );
    ensure!(lease.create_image(8, 1, 1, &[1, 2, 3, 4]).is_err());
    ensure!(lease.validate_handle(7, handle).is_err());
    lease.seal()?;
    lease.validate_handle(7, handle)?;
    ensure!(lease.validate_handle(8, handle).is_err());
    ensure!(
        lease
            .create_text(
                7,
                "x",
                TextLayoutOptions {
                    font_intent: brood_wasm::presentation::FontIntent::Ui,
                    font_weight: brood_wasm::presentation::FontWeight::Normal,
                    font_style: brood_wasm::presentation::FontStyle::Normal,
                    font_size: 12.0,
                    max_width: 20.0,
                    line_height: 14.0,
                    color: [1.0; 4]
                }
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn temporary_text_pressure_discards_partial_resources_and_allows_a_new_invocation() -> Result<()> {
    let backend = Arc::new(FakeBackend::default());
    let registry = registry(Arc::clone(&backend), 2)?;
    let lease = registry.begin(token(), &[7], limits(), budget())?;
    lease.create_image(7, 1, 1, &[1])?;
    *backend
        .allocation_deferred
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    let options = TextLayoutOptions {
        font_intent: brood_wasm::presentation::FontIntent::Monospace,
        font_weight: brood_wasm::presentation::FontWeight::Normal,
        font_style: brood_wasm::presentation::FontStyle::Normal,
        font_size: 12.0,
        max_width: 20.0,
        line_height: 14.0,
        color: [1.0; 4],
    };
    ensure!(lease.create_text(7, "x", options) == Err(PresentationError::AdmissionDeferred));
    ensure!(lease.allocation_deferred());
    ensure!(lease.owned_count() == 1);
    lease.seal()?;
    drop(lease);
    for _ in 0..100 {
        if registry.active_invocations() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    ensure!(registry.active_invocations() == 0);
    ensure!(backend.released() == 1);
    *backend
        .allocation_deferred
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    let retry = registry.begin(token(), &[7], limits(), budget())?;
    let text = retry.create_text(7, "x", options)?;
    ensure!(!retry.allocation_deferred());
    retry.seal()?;
    retry.validate_handle(7, text.handle)?;
    Ok(())
}

#[test]
fn resource_in_use_retries_and_capacity_is_retained_until_release() -> Result<()> {
    let backend = Arc::new(FakeBackend::default());
    backend.set_blocked(true);
    let registry = registry(Arc::clone(&backend), 1)?;
    let lease = registry.begin(token(), &[7], limits(), budget())?;
    lease.create_image(7, 1, 1, &[1, 2, 3, 4])?;
    drop(lease);
    thread::sleep(Duration::from_millis(8));
    ensure!(registry.active_invocations() == 1);
    ensure!(registry.begin(token(), &[7], limits(), budget()).is_err());
    backend.set_blocked(false);
    for _ in 0..100 {
        if registry.active_invocations() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    ensure!(registry.active_invocations() == 0);
    ensure!(backend.released() == 1);
    ensure!(backend.release_calls() <= 10);
    Ok(())
}

#[test]
fn worker_survives_registry_drop_while_external_lease_is_alive() -> Result<()> {
    let backend = Arc::new(FakeBackend::default());
    let registry = registry(Arc::clone(&backend), 1)?;
    let lease = registry.begin(token(), &[7], limits(), budget())?;
    lease.create_image(7, 1, 1, &[1, 2, 3, 4])?;
    drop(registry);
    drop(lease);
    for _ in 0..100 {
        if backend.released() == 1 {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    ensure!(backend.released() == 1);
    ensure!(
        backend
            .release_threads()
            .iter()
            .all(|id| *id != thread::current().id())
    );
    Ok(())
}

#[test]
fn invalidated_budget_revokes_creation_and_validation() -> Result<()> {
    let backend = Arc::new(FakeBackend::default());
    let registry = registry(Arc::clone(&backend), 1)?;
    let retained_budget = budget();
    let lease = registry.begin(token(), &[7], limits(), retained_budget.clone())?;
    retained_budget.invalidate();
    ensure!(lease.create_image(7, 1, 1, &[1, 2, 3, 4]).is_err());
    lease.seal()?;
    ensure!(lease.validate_handle(7, 1).is_err());
    Ok(())
}
