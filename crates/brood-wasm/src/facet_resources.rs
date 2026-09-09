//! Invocation resource ownership retained through off-thread native retirement.

use crate::{
    presentation::{PresentationError, TextLayoutOptions, TextLayoutResult},
    snapshot_budget::{BudgetedValue, ByteBudget},
};
use brood_protocol::facets::{InvocationLimits, RenderToken};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

/// Native allocation must enforce `max_bytes` before retaining resource backing.
/// Temporary text shaping scratch has its own backend limit.
pub trait FacetResourceBackend: fmt::Debug + Send + Sync + 'static {
    fn create_image(
        &self,
        pane: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
        max_bytes: usize,
    ) -> Result<(u64, usize), PresentationError>;
    fn create_text(
        &self,
        pane: u64,
        text: &str,
        options: TextLayoutOptions,
        max_bytes: usize,
    ) -> Result<(u64, TextLayoutResult, usize), PresentationError>;
    fn release(&self, pane: u64, handle: u64) -> Result<(), PresentationError>;
}

#[derive(Clone, Copy, Debug)]
pub struct FacetResourceRegistryConfig {
    pub max_invocations: usize,
    pub retry_interval: Duration,
}

#[derive(Debug)]
struct Resource {
    kind: ResourceKind,
    pane: u64,
    handle: u64,
    bytes: usize,
    guest_released: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Image,
    Text,
}
#[derive(Debug)]
struct State {
    sealed: bool,
    resources: Vec<Resource>,
}
#[derive(Debug)]
struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The reclaimer owns the last reference; final native releases and allocation
/// destruction never run on the caller which drops a frame or invocation.
#[derive(Debug)]
pub struct FacetResourceLease {
    token: RenderToken,
    panes: Vec<u64>,
    limits: InvocationLimits,
    state: Mutex<State>,
    backend: Arc<dyn FacetResourceBackend>,
    reservation: BudgetedValue<()>,
    _budget_owner: ByteBudget,
    _keepalive: SyncSender<Arc<Self>>,
    _slot: Slot,
}

pub struct FacetResourceRegistry {
    sender: Option<SyncSender<Arc<FacetResourceLease>>>,
    active: Arc<AtomicUsize>,
    max_invocations: usize,
    backend: Arc<dyn FacetResourceBackend>,
}

impl FacetResourceRegistry {
    pub fn new(
        backend: Arc<dyn FacetResourceBackend>,
        config: FacetResourceRegistryConfig,
    ) -> Result<Self, PresentationError> {
        if config.max_invocations == 0
            || config.max_invocations > 1024
            || config.retry_interval.is_zero()
        {
            return Err(PresentationError::InvalidArgument);
        }
        let (sender, receiver) = mpsc::sync_channel(config.max_invocations);
        thread::Builder::new()
            .name("brood-facet-resource-reclaimer".into())
            .spawn(move || reclaim(receiver, config))
            .map_err(|_spawn| PresentationError::HostFailure)?;
        Ok(Self {
            sender: Some(sender),
            active: Arc::new(AtomicUsize::new(0)),
            max_invocations: config.max_invocations,
            backend,
        })
    }

    pub fn begin(
        &self,
        token: RenderToken,
        panes: &[u64],
        limits: InvocationLimits,
        budget: ByteBudget,
    ) -> Result<Arc<FacetResourceLease>, PresentationError> {
        if panes.is_empty() || panes.len() > 16 || limits.max_resources > 16_384 {
            return Err(PresentationError::LimitReached);
        }
        if token.scope.actor_generation != budget.generation() || !budget.is_active() {
            return Err(PresentationError::InvalidToken);
        }
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_invocations).then(|| n + 1)
            })
            .map_err(|_capacity| PresentationError::LimitReached)?;
        let slot = Slot(Arc::clone(&self.active));
        let resource_count = usize::try_from(limits.max_resources)
            .map_err(|_size| PresentationError::LimitReached)?;
        let byte_limit = usize::try_from(limits.max_resource_bytes)
            .map_err(|_size| PresentationError::LimitReached)?;
        let metadata = resource_count
            .checked_mul(std::mem::size_of::<Resource>())
            .and_then(|n| n.checked_add(std::mem::size_of_val(panes)))
            .ok_or(PresentationError::LimitReached)?;
        let reservation = BudgetedValue::capture(
            &budget,
            byte_limit
                .checked_add(metadata)
                .ok_or(PresentationError::LimitReached)?,
            || Ok::<_, std::convert::Infallible>(()),
        )
        .map_err(|_budget| PresentationError::LimitReached)?;
        let sender = self
            .sender
            .as_ref()
            .ok_or(PresentationError::ActorStopped)?;
        let lease = Arc::new(FacetResourceLease {
            token,
            panes: panes.to_vec(),
            limits,
            state: Mutex::new(State {
                sealed: false,
                resources: Vec::with_capacity(resource_count),
            }),
            backend: Arc::clone(&self.backend),
            reservation,
            _budget_owner: budget,
            _keepalive: sender.clone(),
            _slot: slot,
        });
        sender
            .try_send(Arc::clone(&lease))
            .map_err(|_registration| PresentationError::HostFailure)?;
        Ok(lease)
    }
    pub fn active_invocations(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

impl Drop for FacetResourceRegistry {
    fn drop(&mut self) {
        drop(self.sender.take());
    }
}

impl FacetResourceLease {
    pub const fn token(&self) -> &RenderToken {
        &self.token
    }
    pub fn owned_count(&self) -> usize {
        self.state
            .lock()
            .map(|s| s.resources.len())
            .unwrap_or_default()
    }
    pub fn create_image(
        &self,
        pane: u64,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<u64, PresentationError> {
        self.create(pane, pixels.len(), ResourceKind::Image, |max| {
            self.backend
                .create_image(pane, width, height, pixels, max)
                .map(|(id, bytes)| (id, id, bytes))
        })
    }
    pub fn create_text(
        &self,
        pane: u64,
        text: &str,
        options: TextLayoutOptions,
    ) -> Result<TextLayoutResult, PresentationError> {
        self.create(pane, text.len(), ResourceKind::Text, |max| {
            self.backend.create_text(pane, text, options, max)
        })
    }
    fn create<T>(
        &self,
        pane: u64,
        minimum: usize,
        kind: ResourceKind,
        allocate: impl FnOnce(usize) -> Result<(u64, T, usize), PresentationError>,
    ) -> Result<T, PresentationError> {
        self.reservation
            .value()
            .map_err(|_revoked| PresentationError::InvalidToken)?;
        if !self.panes.contains(&pane) {
            return Err(PresentationError::InvalidPane);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_poison| PresentationError::HostFailure)?;
        if state.sealed {
            return Err(PresentationError::InvalidToken);
        }
        if state.resources.len() >= self.limits.max_resources as usize {
            return Err(PresentationError::LimitReached);
        }
        let used = state
            .resources
            .iter()
            .try_fold(0usize, |n, r| n.checked_add(r.bytes))
            .ok_or(PresentationError::LimitReached)?;
        let remaining = usize::try_from(self.limits.max_resource_bytes)
            .ok()
            .and_then(|n| n.checked_sub(used))
            .ok_or(PresentationError::LimitReached)?;
        if minimum > remaining {
            return Err(PresentationError::LimitReached);
        }
        let (handle, value, bytes) = allocate(remaining)?;
        if state.resources.iter().any(|r| r.handle == handle) {
            state.sealed = true;
            return Err(PresentationError::HostFailure);
        }
        state.resources.push(Resource {
            kind,
            pane,
            handle,
            bytes,
            guest_released: false,
        });
        if bytes > remaining {
            state.sealed = true;
            return Err(PresentationError::HostFailure);
        }
        drop(state);
        Ok(value)
    }
    pub fn seal(&self) -> Result<(), PresentationError> {
        self.state
            .lock()
            .map(|mut s| s.sealed = true)
            .map_err(|_poison| PresentationError::HostFailure)
    }
    pub fn validate_handle(&self, pane: u64, handle: u64) -> Result<(), PresentationError> {
        self.reservation
            .value()
            .map_err(|_revoked| PresentationError::InvalidToken)?;
        let state = self
            .state
            .lock()
            .map_err(|_poison| PresentationError::HostFailure)?;
        if !state.sealed {
            return Err(PresentationError::InvalidToken);
        }
        if state
            .resources
            .iter()
            .any(|r| r.pane == pane && r.handle == handle && !r.guest_released)
        {
            Ok(())
        } else {
            Err(PresentationError::InvalidResource)
        }
    }
    pub fn validate_kind(
        &self,
        pane: u64,
        handle: u64,
        kind: ResourceKind,
    ) -> Result<(), PresentationError> {
        self.validate_handle(pane, handle)?;
        let state = self
            .state
            .lock()
            .map_err(|_poison| PresentationError::HostFailure)?;
        if state
            .resources
            .iter()
            .any(|r| r.pane == pane && r.handle == handle && r.kind == kind)
        {
            Ok(())
        } else {
            Err(PresentationError::InvalidResource)
        }
    }
    /// Marks an invocation-local temporary as inaccessible. Its native backing
    /// remains bounded and retires on the resource worker with this invocation.
    pub fn drop_resource(&self, pane: u64, handle: u64) -> Result<(), PresentationError> {
        self.reservation
            .value()
            .map_err(|_revoked| PresentationError::InvalidToken)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_poison| PresentationError::HostFailure)?;
        if state.sealed {
            return Err(PresentationError::InvalidToken);
        }
        let resource = state
            .resources
            .iter_mut()
            .find(|r| r.pane == pane && r.handle == handle && !r.guest_released)
            .ok_or(PresentationError::InvalidResource)?;
        resource.guest_released = true;
        drop(state);
        Ok(())
    }
}

fn reclaim(receiver: Receiver<Arc<FacetResourceLease>>, config: FacetResourceRegistryConfig) {
    let mut entries = Vec::with_capacity(config.max_invocations);
    loop {
        match receiver.recv_timeout(config.retry_interval) {
            Ok(lease) => entries.push(lease),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        entries.retain(|lease| {
            if Arc::strong_count(lease) > 1 {
                return true;
            }
            let mut state = lease
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.resources.retain(|resource| {
                !matches!(
                    lease.backend.release(resource.pane, resource.handle),
                    Ok(())
                        | Err(PresentationError::InvalidResource
                            | PresentationError::InvalidPane
                            | PresentationError::ActorStopped)
                )
            });
            !state.resources.is_empty()
        });
    }
}
