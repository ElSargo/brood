use std::time::Duration;

use anyhow::Result;

use crate::{
    ComponentActor, FilesystemWatchStats, PluginLifecycleEvent, Preview1CompatActor,
    Preview1CompatStats,
};

/// One hosted actor generation, independent of the daemon embedding it.
pub enum HostedWasmActor {
    Component(ComponentActor),
    Preview1(Preview1CompatActor),
}

impl HostedWasmActor {
    pub fn try_send_message(&self, sender: String, body: Vec<u8>) -> Result<()> {
        match self {
            Self::Component(actor) => actor.try_send_message(sender, body),
            Self::Preview1(actor) => actor
                .try_send_message(sender, body)
                .map_err(anyhow::Error::from),
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Component(_) => "wasm-component",
            Self::Preview1(_) => "wasm-preview1-compat",
        }
    }

    pub const fn generation(&self) -> u64 {
        match self {
            Self::Component(actor) => actor.generation(),
            Self::Preview1(actor) => actor.generation(),
        }
    }

    pub fn filesystem_watch_stats(&self) -> FilesystemWatchStats {
        match self {
            Self::Component(actor) => actor.filesystem_watch_stats(),
            Self::Preview1(actor) => actor.filesystem_watch_stats(),
        }
    }

    pub async fn lifecycle(&self, event: PluginLifecycleEvent, timeout: Duration) -> Result<()> {
        match self {
            Self::Component(actor) => actor.lifecycle(event, timeout).await,
            Self::Preview1(actor) => actor
                .lifecycle(event, timeout)
                .await
                .map_err(anyhow::Error::from),
        }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::Component(actor) => actor.shutdown().await,
            Self::Preview1(actor) => actor.shutdown().await.map_err(anyhow::Error::from),
        }
    }
}

pub fn aggregate_preview1_compat_stats(
    stats: impl IntoIterator<Item = Preview1CompatStats>,
) -> Option<Preview1CompatStats> {
    let mut stats = stats.into_iter();
    let mut aggregate = stats.next()?;
    let mut instances = 1_usize;
    for next in stats {
        instances = instances.saturating_add(1);
        aggregate.mailbox_queued = aggregate.mailbox_queued.saturating_add(next.mailbox_queued);
        aggregate.mailbox_dropped = aggregate
            .mailbox_dropped
            .saturating_add(next.mailbox_dropped);
        aggregate.mailbox_capacity = aggregate
            .mailbox_capacity
            .saturating_add(next.mailbox_capacity);
        aggregate.mailbox_dispatched = aggregate
            .mailbox_dispatched
            .saturating_add(next.mailbox_dispatched);
        aggregate.completion_delivered = aggregate
            .completion_delivered
            .saturating_add(next.completion_delivered);
        aggregate.notifications_delivered = aggregate
            .notifications_delivered
            .saturating_add(next.notifications_delivered);
        aggregate.notifications = aggregate.notifications.saturating_add(next.notifications);
        aggregate.pollables = aggregate.pollables.saturating_add(next.pollables);
        aggregate.wait_registrations = aggregate
            .wait_registrations
            .saturating_add(next.wait_registrations);
        aggregate.pending_notification_events = aggregate
            .pending_notification_events
            .saturating_add(next.pending_notification_events);
        aggregate.filesystem_watches = aggregate
            .filesystem_watches
            .saturating_add(next.filesystem_watches);
        aggregate.max_filesystem_watches = aggregate
            .max_filesystem_watches
            .saturating_add(next.max_filesystem_watches);
        aggregate.max_filesystem_watch_events = aggregate
            .max_filesystem_watch_events
            .max(next.max_filesystem_watch_events);
        aggregate.filesystem_watch_overflows = aggregate
            .filesystem_watch_overflows
            .saturating_add(next.filesystem_watch_overflows);
        aggregate.filesystem_watch_rescans = aggregate
            .filesystem_watch_rescans
            .saturating_add(next.filesystem_watch_rescans);
        aggregate.workers_started = aggregate
            .workers_started
            .saturating_add(next.workers_started);
        aggregate.workers_active = aggregate.workers_active.saturating_add(next.workers_active);
        aggregate.worker_reads_started = aggregate
            .worker_reads_started
            .saturating_add(next.worker_reads_started);
        aggregate.workers_finished = aggregate
            .workers_finished
            .saturating_add(next.workers_finished);
        aggregate.workers_failed = aggregate.workers_failed.saturating_add(next.workers_failed);
        aggregate.workers_cancelled = aggregate
            .workers_cancelled
            .saturating_add(next.workers_cancelled);
    }
    if instances != 1 {
        // There is no meaningful aggregate generation or latest worker. Keep
        // the legacy flat status truthful while actor diagnostics identify the
        // individual instances.
        aggregate.generation = 0;
        aggregate.last_worker_value = 0;
    }
    Some(aggregate)
}
