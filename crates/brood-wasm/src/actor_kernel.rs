//! Runtime-independent ownership for one hosted actor generation.
//!
//! The Component Model runtime is the canonical actor backend. Preview 1 is a
//! compatibility frontend, but both frontends must share the same generation
//! identity and host-resource lifecycle. Keeping those invariants here avoids
//! giving each ABI its own subtly different actor lifetime.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::{
    coordination::{CoordinationHub, CoordinationStats},
    filesystem_watch::{FilesystemWatchHub, FilesystemWatchStats},
    resource_limits::GenerationByteBudget,
    runtime_io::{RuntimeIoHub, RuntimeIoStats},
};

static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("hosted actor generation allocator is exhausted")]
pub struct GenerationExhausted;

/// Allocate an identity which is unique across every hosted Wasm frontend.
pub fn allocate_generation() -> Result<u64, GenerationExhausted> {
    NEXT_ACTOR_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current != 0).then(|| current.checked_add(1)).flatten()
        })
        .map_err(|_exhausted| GenerationExhausted)
}

/// Host-owned resources whose lifetime is identical for every actor ABI.
pub struct ActorRuntimeKernel {
    generation: u64,
    coordination: CoordinationHub,
    filesystem_watches: FilesystemWatchHub,
    runtime_io: RuntimeIoHub,
    byte_budget: GenerationByteBudget,
}

impl ActorRuntimeKernel {
    pub(crate) fn new(
        generation: u64,
        coordination: CoordinationHub,
        filesystem_watches: FilesystemWatchHub,
        runtime_io: RuntimeIoHub,
        host_byte_capacity: usize,
    ) -> Self {
        Self {
            generation,
            coordination,
            filesystem_watches,
            runtime_io,
            byte_budget: GenerationByteBudget::new(host_byte_capacity),
        }
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn byte_budget(&self) -> &GenerationByteBudget {
        &self.byte_budget
    }

    pub(crate) fn coordination_stats(&self) -> CoordinationStats {
        self.coordination.stats()
    }

    pub(crate) fn filesystem_watch_stats(&self) -> FilesystemWatchStats {
        self.filesystem_watches.stats()
    }

    pub(crate) fn runtime_io_stats(&self) -> RuntimeIoStats {
        self.runtime_io.stats()
    }

    /// Begin teardown without awaiting resources which own async workers.
    pub(crate) fn stop(&self) {
        self.filesystem_watches.stop();
        self.coordination.stop();
    }

    /// Tear down every shared host resource before the ABI-specific runtime is joined.
    pub(crate) async fn shutdown(&self) {
        self.filesystem_watches.stop();
        self.runtime_io.teardown().await;
        self.coordination.stop();
    }
}

impl Drop for ActorRuntimeKernel {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::allocate_generation;

    #[test]
    fn generation_identity_is_shared_by_all_frontends() {
        assert!(matches!(
            (allocate_generation(), allocate_generation()),
            (Ok(first), Ok(second)) if first != second && second == first + 1
        ));
    }
}
