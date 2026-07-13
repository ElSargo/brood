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

/// Enforces the publication order for one replacement transition.
#[derive(Default)]
#[cfg(test)]
pub struct LifecyclePublicationGate {
    old_stopped: bool,
    old_joined: bool,
    candidate_started: bool,
}

#[cfg(test)]
impl LifecyclePublicationGate {
    pub fn old_stopped(&mut self) {
        self.old_stopped = true;
    }
    pub fn old_joined(&mut self) {
        self.old_joined = true;
    }
    pub fn candidate_started(&mut self) {
        self.candidate_started = true;
    }
    pub fn can_publish(&self) -> bool {
        self.old_stopped && self.old_joined && self.candidate_started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_wait_is_bounded_and_preserves_guest_failure() {
        let (_sender, response) = tokio::sync::oneshot::channel();
        assert_eq!(
            wait_for_lifecycle_reply::<String>(response, std::time::Duration::ZERO).await,
            Err(LifecycleWaitError::TimedOut)
        );

        let (sender, response) = tokio::sync::oneshot::channel();
        assert!(sender.send(Err(String::from("guest refused stop"))).is_ok());
        assert_eq!(
            wait_for_lifecycle_reply(response, std::time::Duration::from_secs(1)).await,
            Ok(Err("guest refused stop".into()))
        );
    }

    #[test]
    fn replacement_cannot_publish_before_stop_join_and_start() {
        let mut gate = LifecyclePublicationGate::default();
        assert!(!gate.can_publish());
        gate.old_stopped();
        assert!(!gate.can_publish());
        gate.old_joined();
        assert!(!gate.can_publish());
        gate.candidate_started();
        assert!(gate.can_publish());
    }
}
