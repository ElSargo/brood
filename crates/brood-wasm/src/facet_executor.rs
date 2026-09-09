//! Bounded dedicated execution for isolated render and input contexts.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Result, ensure};
use tokio::sync::oneshot;

use crate::{
    facet_resources::FacetResourceLease,
    facet_runtime::{FacetOperation, FacetRuntime, FacetRuntimeConfig},
    snapshot_budget::{BudgetedBytes, ByteBudget},
};

/// Shared admission service. Context slots remain occupied through actual
/// worker exit, including an interrupted guest call's bounded unwind.
#[derive(Clone)]
pub struct FacetExecutor {
    active: Arc<AtomicUsize>,
    max_contexts: usize,
    queue_capacity: usize,
}

struct ContextSlot(Arc<AtomicUsize>);
impl Drop for ContextSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Invocation {
    operation: FacetOperation,
    input: BudgetedBytes,
    output_budget: ByteBudget,
    completion: oneshot::Sender<Result<BudgetedBytes>>,
    resources: Option<Arc<FacetResourceLease>>,
}

/// Dropping the handle requests shutdown without joining a guest worker on the
/// caller's thread. Fuel and epochs bound the final in-progress guest call.
pub struct FacetWorker {
    commands: mpsc::SyncSender<Invocation>,
    stopped: Arc<AtomicBool>,
}

impl FacetExecutor {
    pub fn new(max_contexts: usize, queue_capacity: usize) -> Result<Self> {
        ensure!(
            max_contexts > 0 && queue_capacity > 0,
            "facet executor limits must be nonzero"
        );
        Ok(Self {
            active: Arc::new(AtomicUsize::new(0)),
            max_contexts,
            queue_capacity,
        })
    }

    pub fn active_contexts(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Compilation and instantiation happen on the admitted worker, never on a
    /// presentation event loop or its asynchronous executor.
    pub async fn load(
        &self,
        path: PathBuf,
        config: FacetRuntimeConfig,
    ) -> Result<Option<FacetWorker>> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.max_contexts).then(|| active + 1)
            })
            .map_err(|_active| anyhow::anyhow!("facet context capacity exhausted"))?;
        let slot = ContextSlot(Arc::clone(&self.active));
        let (commands, receiver) = mpsc::sync_channel::<Invocation>(self.queue_capacity);
        let (ready, loaded) = oneshot::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        thread::Builder::new()
            .name("brood-facet".to_owned())
            .spawn(move || {
                let _slot = slot;
                let mut runtime = match FacetRuntime::load(path, &config) {
                    Ok(Some(runtime)) => runtime,
                    Ok(None) => {
                        let _sent = ready.send(Ok(false));
                        return;
                    }
                    Err(error) => {
                        let _sent = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(true)).is_err() {
                    return;
                }
                while let Ok(invocation) = receiver.recv() {
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    if invocation.completion.is_closed() {
                        continue;
                    }
                    let result = invocation
                        .input
                        .bytes()
                        .map_err(anyhow::Error::from)
                        .and_then(|input| match invocation.resources {
                            Some(resources) => runtime.invoke_with_resources(
                                input,
                                &invocation.output_budget,
                                resources,
                            ),
                            None => runtime.invoke(
                                invocation.operation,
                                input,
                                &invocation.output_budget,
                            ),
                        });
                    if !runtime.is_usable() {
                        worker_stopped.store(true, Ordering::Release);
                        let _sent = invocation.completion.send(result);
                        break;
                    }
                    let _sent = invocation.completion.send(result);
                }
                worker_stopped.store(true, Ordering::Release);
            })?;
        if loaded
            .await
            .map_err(|_closed| anyhow::anyhow!("facet worker stopped while loading"))??
        {
            Ok(Some(FacetWorker { commands, stopped }))
        } else {
            Ok(None)
        }
    }
}

impl FacetWorker {
    /// Admission never waits for another guest invocation. The input allocation
    /// carries its reservation across the queue and through the guest transfer.
    pub fn try_invoke(
        &self,
        operation: FacetOperation,
        input: BudgetedBytes,
        output_budget: ByteBudget,
    ) -> Result<oneshot::Receiver<Result<BudgetedBytes>>> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "facet worker is unavailable"
        );
        ensure!(
            input.bytes().is_ok() && output_budget.is_active(),
            "facet invocation authority was invalidated"
        );
        let (completion, receiver) = oneshot::channel();
        self.commands
            .try_send(Invocation {
                operation,
                input,
                output_budget,
                completion,
                resources: None,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => anyhow::anyhow!("facet invocation queue is full"),
                mpsc::TrySendError::Disconnected(_) => {
                    anyhow::anyhow!("facet worker is unavailable")
                }
            })?;
        Ok(receiver)
    }

    /// Queue a render with one invocation-scoped graphics resource lease.
    pub fn try_invoke_with_resources(
        &self,
        input: BudgetedBytes,
        output_budget: ByteBudget,
        resources: Arc<FacetResourceLease>,
    ) -> Result<oneshot::Receiver<Result<BudgetedBytes>>> {
        ensure!(
            !self.stopped.load(Ordering::Acquire),
            "facet worker is unavailable"
        );
        ensure!(
            input.bytes().is_ok() && output_budget.is_active(),
            "facet invocation authority was invalidated"
        );
        let (completion, receiver) = oneshot::channel();
        self.commands
            .try_send(Invocation {
                operation: FacetOperation::Render,
                input,
                output_budget,
                completion,
                resources: Some(resources),
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => anyhow::anyhow!("facet invocation queue is full"),
                mpsc::TrySendError::Disconnected(_) => {
                    anyhow::anyhow!("facet worker is unavailable")
                }
            })?;
        Ok(receiver)
    }
}

impl Drop for FacetWorker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}
