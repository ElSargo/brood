use std::{collections::VecDeque, sync::Arc};

use anyhow::{Result, bail};
use smithay::reexports::calloop::channel;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(crate) const DEBUG_TOGGLE_ACTOR_ID: &str = "debug-toggle";
pub(crate) const HOST_ACTOR_ID: &str = "host";
const DEBUG_TOGGLE_ACTOR_KIND: &str = "builtin-toggle-launcher";
const HOST_ACTOR_KIND: &str = "native-host";
const DEBUG_TOGGLE_ACTOR_MAILBOX_CAPACITY: usize = 64;
const HOST_ACTOR_MAILBOX_CAPACITY: usize = 256;
const DEFAULT_MAILBOX_SLOT: &str = "default";
const DEFAULT_PUBLICATION_SLOT: &str = "default";
const MAX_LINKED_FRAME_DEPTH: usize = 16;

#[derive(Clone, Debug)]
pub(crate) struct PluginMessage {
    pub(crate) sender: String,
    pub(crate) body: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginHostMessage {
    pub(crate) sender: String,
    pub(crate) body: String,
}

#[derive(Debug)]
pub(crate) enum PluginHostEvent {
    AppendPublicationFrame {
        actor_id: String,
        slot: String,
        frame: PluginFramePayload,
    },
    DrainWasmHostMailbox,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginMailboxSummary {
    pub(crate) slot: String,
    pub(crate) enqueued_messages: u64,
    pub(crate) dropped_messages: u64,
    pub(crate) capacity: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginActorSummary {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) enqueued_messages: u64,
    pub(crate) dropped_messages: u64,
    pub(crate) mailbox_capacity: usize,
    pub(crate) mailbox_slots: Vec<PluginMailboxSummary>,
    pub(crate) published_frames: u64,
    pub(crate) latest_frame: String,
}

#[derive(Debug)]
pub(crate) struct PluginRuntime {
    actors: Vec<PluginActorHandle>,
    host_mailbox: PluginHostMailbox,
}

#[derive(Debug)]
struct PluginActorHandle {
    id: String,
    kind: String,
    mailboxes: Vec<PluginMailboxHandle>,
    publications: Vec<PluginPublication>,
}

#[derive(Debug)]
struct PluginMailboxHandle {
    slot: String,
    sender: mpsc::Sender<PluginMessage>,
    enqueued_messages: u64,
    dropped_messages: u64,
    capacity: usize,
}

#[derive(Debug)]
struct PluginHostMailbox {
    messages: VecDeque<PluginHostMessage>,
    enqueued_messages: u64,
    dropped_messages: u64,
    capacity: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum PluginFramePayload {
    ToggleChoice { spawn_btop_next: bool },
}

#[derive(Debug)]
struct PluginFrameNode {
    payload: PluginFramePayload,
    previous: Option<Arc<PluginFrameNode>>,
    depth: usize,
}

#[derive(Debug)]
struct PluginPublication {
    slot: String,
    latest: Option<Arc<PluginFrameNode>>,
    published_frames: u64,
}

impl PluginRuntime {
    pub(crate) fn start_builtin(
        runtime: &tokio::runtime::Handle,
        host_sender: channel::Sender<PluginHostEvent>,
    ) -> Self {
        Self {
            actors: vec![start_toggle_actor(runtime, host_sender)],
            host_mailbox: PluginHostMailbox::new(HOST_ACTOR_MAILBOX_CAPACITY),
        }
    }

    pub(crate) fn append_frame(
        &mut self,
        actor_id: &str,
        slot: &str,
        frame: PluginFramePayload,
    ) -> Result<()> {
        let Some(actor) = self.actor_mut(actor_id) else {
            bail!("unknown plugin actor {actor_id:?}");
        };

        actor.append_frame(slot, frame);
        Ok(())
    }

    pub(crate) fn send_message(
        &mut self,
        actor_id: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        self.send_message_to_slot(actor_id, DEFAULT_MAILBOX_SLOT, sender, body)
    }

    pub(crate) fn send_message_to_slot(
        &mut self,
        actor_id: &str,
        slot: &str,
        sender: String,
        body: String,
    ) -> Result<()> {
        if actor_id == HOST_ACTOR_ID {
            return self.host_mailbox.send(sender, body);
        }

        let Some(actor) = self.actor_mut(actor_id) else {
            bail!("unknown plugin actor {actor_id:?}");
        };

        actor.send_message_to_slot(slot, PluginMessage { sender, body })
    }

    pub(crate) fn mailbox_summaries(&self, actor_id: &str) -> Result<Vec<PluginMailboxSummary>> {
        if actor_id == HOST_ACTOR_ID {
            return Ok(self.host_mailbox.summary().mailbox_slots);
        }

        let Some(actor) = self.actor(actor_id) else {
            bail!("unknown plugin actor {actor_id:?}");
        };
        Ok(actor.mailbox_summaries())
    }

    pub(crate) fn actor_summaries(&self) -> Vec<PluginActorSummary> {
        let mut summaries = Vec::with_capacity(self.actors.len() + 1);
        summaries.push(self.host_mailbox.summary());
        summaries.extend(self.actors.iter().map(PluginActorHandle::summary));
        summaries
    }

    pub(crate) fn drain_host_messages(&mut self) -> Vec<PluginHostMessage> {
        self.host_mailbox.drain()
    }

    fn actor(&self, actor_id: &str) -> Option<&PluginActorHandle> {
        self.actors
            .iter()
            .find(|actor| actor.id.as_str() == actor_id)
    }

    fn actor_mut(&mut self, actor_id: &str) -> Option<&mut PluginActorHandle> {
        self.actors
            .iter_mut()
            .find(|actor| actor.id.as_str() == actor_id)
    }
}

impl PluginActorHandle {
    fn new(id: String, kind: String, mailboxes: Vec<PluginMailboxHandle>) -> Self {
        Self {
            id,
            kind,
            mailboxes,
            publications: Vec::new(),
        }
    }

    fn send_message_to_slot(&mut self, slot: &str, message: PluginMessage) -> Result<()> {
        let actor_id = self.id.clone();
        let Some(mailbox) = self
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.slot == slot)
        else {
            bail!("plugin actor {actor_id:?} has no mailbox slot {slot:?}");
        };

        mailbox.send(&actor_id, message)
    }

    fn append_frame(&mut self, slot: &str, frame: PluginFramePayload) {
        if let Some(publication) = self
            .publications
            .iter_mut()
            .find(|publication| publication.slot == slot)
        {
            publication.append(frame);
            return;
        }

        self.publications
            .push(PluginPublication::new(slot.to_string()));
        if let Some(publication) = self.publications.last_mut() {
            publication.append(frame);
        }
    }

    fn summary(&self) -> PluginActorSummary {
        let mailbox_slots = self.mailbox_summaries();
        PluginActorSummary {
            id: self.id.clone(),
            kind: self.kind.clone(),
            enqueued_messages: mailbox_slots
                .iter()
                .map(|mailbox| mailbox.enqueued_messages)
                .sum(),
            dropped_messages: mailbox_slots
                .iter()
                .map(|mailbox| mailbox.dropped_messages)
                .sum(),
            mailbox_capacity: mailbox_slots.iter().map(|mailbox| mailbox.capacity).sum(),
            mailbox_slots,
            published_frames: self.published_frames(),
            latest_frame: self.latest_frame_label(),
        }
    }

    fn mailbox_summaries(&self) -> Vec<PluginMailboxSummary> {
        self.mailboxes
            .iter()
            .map(PluginMailboxHandle::summary)
            .collect()
    }

    fn published_frames(&self) -> u64 {
        self.publications
            .iter()
            .map(|publication| publication.published_frames)
            .sum()
    }

    fn latest_frame_label(&self) -> String {
        self.publications
            .iter()
            .find(|publication| publication.slot == DEFAULT_PUBLICATION_SLOT)
            .map_or_else(|| "none".to_string(), PluginPublication::latest_label)
    }
}

impl PluginHostMailbox {
    const fn new(capacity: usize) -> Self {
        Self {
            messages: VecDeque::new(),
            enqueued_messages: 0,
            dropped_messages: 0,
            capacity,
        }
    }

    fn send(&mut self, sender: String, body: String) -> Result<()> {
        if self.messages.len() >= self.capacity {
            self.dropped_messages += 1;
            bail!("plugin host mailbox rejected message: mailbox is full");
        }
        self.messages.push_back(PluginHostMessage { sender, body });
        self.enqueued_messages += 1;
        Ok(())
    }

    fn drain(&mut self) -> Vec<PluginHostMessage> {
        self.messages.drain(..).collect()
    }

    fn summary(&self) -> PluginActorSummary {
        PluginActorSummary {
            id: HOST_ACTOR_ID.to_string(),
            kind: HOST_ACTOR_KIND.to_string(),
            enqueued_messages: self.enqueued_messages,
            dropped_messages: self.dropped_messages,
            mailbox_capacity: self.capacity,
            mailbox_slots: vec![PluginMailboxSummary {
                slot: DEFAULT_MAILBOX_SLOT.to_string(),
                enqueued_messages: self.enqueued_messages,
                dropped_messages: self.dropped_messages,
                capacity: self.capacity,
            }],
            published_frames: 0,
            latest_frame: "none".to_string(),
        }
    }
}

impl PluginMailboxHandle {
    fn new(slot: String, capacity: usize) -> (Self, mpsc::Receiver<PluginMessage>) {
        let (sender, receiver) = mpsc::channel::<PluginMessage>(capacity);
        (
            Self {
                slot,
                sender,
                enqueued_messages: 0,
                dropped_messages: 0,
                capacity,
            },
            receiver,
        )
    }

    fn send(&mut self, actor_id: &str, message: PluginMessage) -> Result<()> {
        match self.sender.try_send(message) {
            Ok(()) => {
                self.enqueued_messages += 1;
                Ok(())
            }
            Err(err) => {
                self.dropped_messages += 1;
                let slot = &self.slot;
                bail!("plugin actor {actor_id:?} mailbox slot {slot:?} rejected message: {err}")
            }
        }
    }

    fn summary(&self) -> PluginMailboxSummary {
        PluginMailboxSummary {
            slot: self.slot.clone(),
            enqueued_messages: self.enqueued_messages,
            dropped_messages: self.dropped_messages,
            capacity: self.capacity,
        }
    }
}

impl PluginFramePayload {
    fn label(&self) -> String {
        match self {
            Self::ToggleChoice { spawn_btop_next } => {
                format!("toggle_choice:spawn_btop_next={spawn_btop_next}")
            }
        }
    }
}

impl PluginPublication {
    fn new(slot: String) -> Self {
        Self {
            slot,
            latest: None,
            published_frames: 0,
        }
    }

    fn append(&mut self, payload: PluginFramePayload) {
        let previous = self.latest.as_ref().and_then(|latest| {
            (latest.depth + 1 < MAX_LINKED_FRAME_DEPTH).then(|| Arc::clone(latest))
        });
        let depth = previous.as_ref().map_or(1, |previous| previous.depth + 1);
        self.latest = Some(Arc::new(PluginFrameNode {
            payload,
            previous,
            depth,
        }));
        self.published_frames += 1;
    }

    fn latest_label(&self) -> String {
        self.latest.as_ref().map_or_else(
            || "none".to_string(),
            |latest| {
                format!(
                    "{}:retained_depth={}",
                    latest.payload.label(),
                    latest.retained_depth()
                )
            },
        )
    }
}

impl PluginFrameNode {
    fn retained_depth(&self) -> usize {
        let mut depth = 1;
        let mut current = self.previous.as_deref();
        while let Some(node) = current {
            depth += 1;
            current = node.previous.as_deref();
        }
        depth
    }
}

fn start_toggle_actor(
    runtime: &tokio::runtime::Handle,
    host_sender: channel::Sender<PluginHostEvent>,
) -> PluginActorHandle {
    let (default_mailbox, mut default_receiver) = PluginMailboxHandle::new(
        DEFAULT_MAILBOX_SLOT.to_string(),
        DEBUG_TOGGLE_ACTOR_MAILBOX_CAPACITY,
    );
    let actor_id = DEBUG_TOGGLE_ACTOR_ID.to_string();
    let task_actor_id = actor_id.clone();
    let task_host_sender = host_sender.clone();
    let task = runtime.spawn(async move {
        info!(plugin_actor = %task_actor_id, "plugin actor started");
        let mut spawn_btop_next = false;
        publish_toggle_frame(&task_host_sender, &task_actor_id, spawn_btop_next);
        while let Some(message) = default_receiver.recv().await {
            info!(
                plugin_actor = %task_actor_id,
                mailbox_slot = %DEFAULT_MAILBOX_SLOT,
                dbus_sender = %message.sender,
                message = %message.body,
                "plugin actor received mailbox message"
            );
            spawn_btop_next = !spawn_btop_next;
            publish_toggle_frame(&task_host_sender, &task_actor_id, spawn_btop_next);
        }
        info!(plugin_actor = %task_actor_id, "plugin actor stopped");
    });
    drop(task);

    PluginActorHandle::new(
        actor_id,
        DEBUG_TOGGLE_ACTOR_KIND.to_string(),
        vec![default_mailbox],
    )
}

fn publish_toggle_frame(
    host_sender: &channel::Sender<PluginHostEvent>,
    actor_id: &str,
    spawn_btop_next: bool,
) {
    if let Err(err) = host_sender.send(PluginHostEvent::AppendPublicationFrame {
        actor_id: actor_id.to_string(),
        slot: DEFAULT_PUBLICATION_SLOT.to_string(),
        frame: PluginFramePayload::ToggleChoice { spawn_btop_next },
    }) {
        warn!(plugin_actor = %actor_id, error = ?err, "plugin actor could not publish publication frame");
    }
}
