use anyhow::{Result, bail};
use smithay::reexports::calloop::channel;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(crate) const DEFAULT_FOOT_ACTOR_ID: &str = "debug-foot";
const DEFAULT_FOOT_ACTOR_KIND: &str = "builtin-foot-launcher";
const DEFAULT_FOOT_ACTOR_MAILBOX_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct PluginMessage {
    pub(crate) sender: String,
    pub(crate) body: String,
}

#[derive(Debug)]
pub(crate) enum PluginHostEvent {
    SpawnFootRequested {
        actor_id: String,
        message: PluginMessage,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct PluginActorSummary {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) enqueued_messages: u64,
    pub(crate) dropped_messages: u64,
    pub(crate) mailbox_capacity: usize,
}

#[derive(Debug)]
pub(crate) struct PluginRuntime {
    actors: Vec<PluginActorHandle>,
}

#[derive(Debug)]
struct PluginActorHandle {
    id: String,
    kind: String,
    mailbox: mpsc::Sender<PluginMessage>,
    enqueued_messages: u64,
    dropped_messages: u64,
    mailbox_capacity: usize,
}

impl PluginRuntime {
    pub(crate) fn start_builtin(
        runtime: &tokio::runtime::Handle,
        host_sender: channel::Sender<PluginHostEvent>,
    ) -> Self {
        Self {
            actors: vec![start_foot_actor(runtime, host_sender)],
        }
    }

    pub(crate) fn send_message(
        &mut self,
        actor_id: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        let Some(actor) = self
            .actors
            .iter_mut()
            .find(|actor| actor.id.as_str() == actor_id)
        else {
            bail!("unknown plugin actor {actor_id:?}");
        };

        let message = PluginMessage { sender, body };
        match actor.mailbox.try_send(message) {
            Ok(()) => {
                actor.enqueued_messages += 1;
                Ok(())
            }
            Err(err) => {
                actor.dropped_messages += 1;
                bail!("plugin actor {actor_id:?} mailbox rejected message: {err}")
            }
        }
    }

    pub(crate) fn actor_summaries(&self) -> Vec<PluginActorSummary> {
        self.actors
            .iter()
            .map(|actor| PluginActorSummary {
                id: actor.id.clone(),
                kind: actor.kind.clone(),
                enqueued_messages: actor.enqueued_messages,
                dropped_messages: actor.dropped_messages,
                mailbox_capacity: actor.mailbox_capacity,
            })
            .collect()
    }
}

fn start_foot_actor(
    runtime: &tokio::runtime::Handle,
    host_sender: channel::Sender<PluginHostEvent>,
) -> PluginActorHandle {
    let (mailbox, mut receiver) =
        mpsc::channel::<PluginMessage>(DEFAULT_FOOT_ACTOR_MAILBOX_CAPACITY);
    let actor_id = DEFAULT_FOOT_ACTOR_ID.to_string();
    let task_actor_id = actor_id.clone();
    let task = runtime.spawn(async move {
        info!(plugin_actor = %task_actor_id, "plugin actor started");
        while let Some(message) = receiver.recv().await {
            info!(
                plugin_actor = %task_actor_id,
                dbus_sender = %message.sender,
                "plugin actor received mailbox message"
            );
            if let Err(err) = host_sender.send(PluginHostEvent::SpawnFootRequested {
                actor_id: task_actor_id.clone(),
                message,
            }) {
                warn!(plugin_actor = %task_actor_id, error = ?err, "plugin actor could not notify host");
                break;
            }
        }
        info!(plugin_actor = %task_actor_id, "plugin actor stopped");
    });
    drop(task);

    PluginActorHandle {
        id: actor_id,
        kind: DEFAULT_FOOT_ACTOR_KIND.to_string(),
        mailbox,
        enqueued_messages: 0,
        dropped_messages: 0,
        mailbox_capacity: DEFAULT_FOOT_ACTOR_MAILBOX_CAPACITY,
    }
}
