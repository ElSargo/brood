//! Typed hosted-plugin lifecycle context shared by Component and Preview 1 actors.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginStartReason {
    Initial,
    Replacement,
    SessionRebind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginStopReason {
    Replacement,
    SessionRebind,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginLifecycleEntry {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStartContext {
    pub instance_id: String,
    pub generation: u64,
    pub reason: PluginStartReason,
    pub backend_id: Option<u64>,
    pub session_id: Option<u64>,
    pub workspace_root: String,
    pub data: Vec<PluginLifecycleEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStopContext {
    pub instance_id: String,
    pub generation: u64,
    pub reason: PluginStopReason,
    pub next_generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginLifecycleEvent {
    Start(PluginStartContext),
    Stop(PluginStopContext),
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum LifecycleWaitError {
    #[error("lifecycle hook timed out")]
    TimedOut,
    #[error("lifecycle hook reply dropped")]
    ReplyDropped,
}

pub async fn wait_for_lifecycle_reply<E>(
    response: tokio::sync::oneshot::Receiver<Result<(), E>>,
    timeout: std::time::Duration,
) -> Result<Result<(), E>, LifecycleWaitError> {
    tokio::time::timeout(timeout, response)
        .await
        .map_err(|_elapsed| LifecycleWaitError::TimedOut)?
        .map_err(|_closed| LifecycleWaitError::ReplyDropped)
}
