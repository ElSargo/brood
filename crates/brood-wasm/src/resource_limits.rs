use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};

/// Deliberately generous fault-containment ceilings for one plugin generation.
///
/// These are daemon safety boundaries, not tuning targets. Queue item limits
/// remain responsible for normal backpressure; these byte limits ensure that a
/// bounded number of pathological items cannot retain unbounded host memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WasmResourceLimits {
    /// Maximum UTF-8 bytes in actor mailbox sender metadata.
    pub mailbox_sender_bytes: usize,
    /// Maximum bytes in one actor mailbox body.
    pub mailbox_body_bytes: usize,
    /// Maximum bytes passed to one Component worker invocation.
    pub worker_payload_bytes: usize,
    /// Maximum bytes returned by one Component worker, including guest errors.
    pub worker_result_bytes: usize,
    /// Maximum bytes retained in host-side actor queues for one generation.
    pub generation_host_bytes: usize,
    /// Maximum aggregate linear-memory bytes retained by one actor generation.
    pub generation_memory_bytes: usize,
    /// Maximum table elements retained by one Wasmtime store.
    pub store_table_elements: usize,
}

impl Default for WasmResourceLimits {
    fn default() -> Self {
        Self {
            mailbox_sender_bytes: 64 * 1024,
            mailbox_body_bytes: 16 * 1024 * 1024,
            worker_payload_bytes: 16 * 1024 * 1024,
            worker_result_bytes: 16 * 1024 * 1024,
            generation_host_bytes: 256 * 1024 * 1024,
            generation_memory_bytes: 512 * 1024 * 1024,
            store_table_elements: 1024 * 1024,
        }
    }
}

impl WasmResourceLimits {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            self.mailbox_sender_bytes > 0,
            "mailbox sender byte limit must be non-zero"
        );
        ensure!(
            self.mailbox_body_bytes > 0,
            "mailbox body byte limit must be non-zero"
        );
        ensure!(
            self.worker_payload_bytes > 0,
            "worker payload byte limit must be non-zero"
        );
        ensure!(
            self.worker_result_bytes > 0,
            "worker result byte limit must be non-zero"
        );
        ensure!(
            self.generation_host_bytes > 0,
            "generation host byte limit must be non-zero"
        );
        ensure!(
            self.generation_memory_bytes > 0,
            "generation memory byte limit must be non-zero"
        );
        ensure!(
            self.store_table_elements > 0,
            "store table element limit must be non-zero"
        );
        Ok(())
    }

    pub(crate) fn store_limits(self) -> GenerationStoreLimits {
        GenerationStoreLimits {
            inner: StoreLimitsBuilder::new()
                .memory_size(self.generation_memory_bytes)
                .table_elements(self.store_table_elements)
                .build(),
            memory_limit: self.generation_memory_bytes,
            memory_bytes: 0,
            pending_memory_growth: 0,
            table_limit: self.store_table_elements,
            table_elements: 0,
            pending_table_growth: 0,
        }
    }

    pub(crate) fn ensure_mailbox(self, sender: &str, body: &[u8]) -> Result<()> {
        ensure!(
            sender.len() <= self.mailbox_sender_bytes,
            "mailbox sender exceeds {} byte limit",
            self.mailbox_sender_bytes
        );
        ensure!(
            body.len() <= self.mailbox_body_bytes,
            "mailbox body exceeds {} byte limit",
            self.mailbox_body_bytes
        );
        Ok(())
    }

    pub(crate) fn ensure_worker_payload(self, payload: &[u8]) -> Result<()> {
        ensure!(
            payload.len() <= self.worker_payload_bytes,
            "worker payload exceeds {} byte limit",
            self.worker_payload_bytes
        );
        Ok(())
    }

    pub(crate) fn ensure_worker_result(self, result: &[u8]) -> Result<()> {
        ensure!(
            result.len() <= self.worker_result_bytes,
            "worker result exceeds {} byte limit",
            self.worker_result_bytes
        );
        Ok(())
    }
}

/// Store limiter which applies aggregate memory and table-element ceilings
/// across all resources in one component instance.
#[derive(Debug)]
pub struct GenerationStoreLimits {
    inner: StoreLimits,
    memory_limit: usize,
    memory_bytes: usize,
    pending_memory_growth: usize,
    table_limit: usize,
    table_elements: usize,
    pending_table_growth: usize,
}

impl ResourceLimiter for GenerationStoreLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let additional = desired.saturating_sub(current);
        if additional > self.memory_limit.saturating_sub(self.memory_bytes) {
            return Ok(false);
        }
        if !self.inner.memory_growing(current, desired, maximum)? {
            return Ok(false);
        }
        self.memory_bytes += additional;
        self.pending_memory_growth = additional;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_bytes = self.memory_bytes.saturating_sub(self.pending_memory_growth);
        self.pending_memory_growth = 0;
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let additional = desired.saturating_sub(current);
        self.pending_table_growth = 0;
        if additional > self.table_limit.saturating_sub(self.table_elements) {
            return Ok(false);
        }
        if !self.inner.table_growing(current, desired, maximum)? {
            return Ok(false);
        }
        self.table_elements += additional;
        self.pending_table_growth = additional;
        Ok(true)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.table_elements = self
            .table_elements
            .saturating_sub(self.pending_table_growth);
        self.pending_table_growth = 0;
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

#[derive(Clone, Debug)]
pub struct GenerationByteBudget {
    inner: Arc<Mutex<ByteBudgetState>>,
}

#[derive(Debug)]
struct ByteBudgetState {
    available: usize,
}

#[derive(Debug)]
pub struct GenerationBytePermit {
    budget: GenerationByteBudget,
    bytes: usize,
}

impl GenerationByteBudget {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ByteBudgetState {
                available: capacity,
            })),
        }
    }

    pub(crate) fn try_reserve(&self, bytes: usize) -> Result<GenerationBytePermit> {
        let mut state = self
            .inner
            .lock()
            .map_err(|error| anyhow::anyhow!("generation byte budget is poisoned: {error}"))?;
        ensure!(
            bytes <= state.available,
            "generation host byte budget is full"
        );
        state.available -= bytes;
        drop(state);
        Ok(GenerationBytePermit {
            budget: self.clone(),
            bytes,
        })
    }
}

impl Drop for GenerationBytePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.budget.inner.lock() {
            state.available = state.available.saturating_add(self.bytes);
        }
    }
}
