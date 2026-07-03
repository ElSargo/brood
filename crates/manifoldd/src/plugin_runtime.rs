use std::sync::Arc;

use anyhow::{Result, bail};
use smithay::{
    backend::input::{KeyState, Keycode},
    reexports::calloop::channel,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub(crate) const DEBUG_TOGGLE_ACTOR_ID: &str = "debug-toggle";
const DEBUG_TOGGLE_ACTOR_KIND: &str = "builtin-toggle-launcher";
const DEBUG_TOGGLE_ACTOR_MAILBOX_CAPACITY: usize = 64;
const DEFAULT_SWAPCHAIN_SLOT: &str = "default";
const MAX_LINKED_FRAME_DEPTH: usize = 16;
const SYNC_KEY_SENDER: &str = "host:sync-key";
const KEY_O: Keycode = Keycode::new(32);

#[derive(Clone, Debug)]
pub(crate) struct PluginMessage {
    pub(crate) sender: String,
    pub(crate) body: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginActorMessage {
    pub(crate) actor_id: String,
    pub(crate) sender: String,
    pub(crate) body: String,
}

#[derive(Debug)]
pub(crate) enum PluginHostEvent {
    AppendSwapchainFrame {
        actor_id: String,
        slot: String,
        frame: PluginFramePayload,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct PluginActorSummary {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) enqueued_messages: u64,
    pub(crate) dropped_messages: u64,
    pub(crate) mailbox_capacity: usize,
    pub(crate) published_frames: u64,
    pub(crate) latest_frame: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginSyncKeyInput {
    pub(crate) key_code: Keycode,
    pub(crate) key_state: KeyState,
    pub(crate) ctrl_pressed: bool,
    pub(crate) alt_pressed: bool,
    pub(crate) shift_pressed: bool,
}

#[derive(Debug)]
pub(crate) struct PluginSyncKeyDecision {
    pub(crate) spawn_argv: Option<Vec<String>>,
    pub(crate) actor_messages: Vec<PluginActorMessage>,
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
    swapchains: Vec<PluginSwapchain>,
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
struct PluginSwapchain {
    slot: String,
    latest: Option<Arc<PluginFrameNode>>,
    appended_frames: u64,
}

impl PluginRuntime {
    pub(crate) fn start_builtin(
        runtime: &tokio::runtime::Handle,
        host_sender: channel::Sender<PluginHostEvent>,
    ) -> Self {
        Self {
            actors: vec![start_toggle_actor(runtime, host_sender)],
        }
    }

    pub(crate) fn append_frame(
        &mut self,
        actor_id: &str,
        slot: &str,
        frame: PluginFramePayload,
    ) -> Result<()> {
        let Some(actor) = self
            .actors
            .iter_mut()
            .find(|actor| actor.id.as_str() == actor_id)
        else {
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

    pub(crate) fn handle_sync_key(
        &self,
        input: &PluginSyncKeyInput,
    ) -> Option<PluginSyncKeyDecision> {
        if input.key_code != KEY_O
            || !input.ctrl_pressed
            || input.alt_pressed
            || input.shift_pressed
        {
            return None;
        }

        if input.key_state == KeyState::Released {
            return Some(PluginSyncKeyDecision {
                spawn_argv: None,
                actor_messages: Vec::new(),
            });
        }

        let spawn_btop = self
            .actor(DEBUG_TOGGLE_ACTOR_ID)
            .and_then(|actor| actor.latest_payload(DEFAULT_SWAPCHAIN_SLOT))
            .is_some_and(|payload| match payload {
                PluginFramePayload::ToggleChoice { spawn_btop_next } => spawn_btop_next,
            });
        let tool = if spawn_btop { "btop" } else { "htop" };

        Some(PluginSyncKeyDecision {
            spawn_argv: Some(vec!["foot".to_string(), tool.to_string()]),
            actor_messages: vec![PluginActorMessage {
                actor_id: DEBUG_TOGGLE_ACTOR_ID.to_string(),
                sender: SYNC_KEY_SENDER.to_string(),
                body: "toggle".to_string(),
            }],
        })
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
                published_frames: actor.published_frames(),
                latest_frame: actor.latest_frame_label(),
            })
            .collect()
    }

    fn actor(&self, actor_id: &str) -> Option<&PluginActorHandle> {
        self.actors
            .iter()
            .find(|actor| actor.id.as_str() == actor_id)
    }
}

impl PluginActorHandle {
    fn append_frame(&mut self, slot: &str, frame: PluginFramePayload) {
        if let Some(swapchain) = self
            .swapchains
            .iter_mut()
            .find(|swapchain| swapchain.slot == slot)
        {
            swapchain.append(frame);
            return;
        }

        self.swapchains.push(PluginSwapchain::new(slot.to_string()));
        if let Some(swapchain) = self.swapchains.last_mut() {
            swapchain.append(frame);
        }
    }

    fn latest_payload(&self, slot: &str) -> Option<PluginFramePayload> {
        self.swapchains
            .iter()
            .find(|swapchain| swapchain.slot == slot)
            .and_then(PluginSwapchain::latest_payload)
    }

    fn published_frames(&self) -> u64 {
        self.swapchains
            .iter()
            .map(|swapchain| swapchain.appended_frames)
            .sum()
    }

    fn latest_frame_label(&self) -> String {
        self.swapchains
            .iter()
            .find(|swapchain| swapchain.slot == DEFAULT_SWAPCHAIN_SLOT)
            .map_or_else(|| "none".to_string(), PluginSwapchain::latest_label)
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

impl PluginSwapchain {
    fn new(slot: String) -> Self {
        Self {
            slot,
            latest: None,
            appended_frames: 0,
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
        self.appended_frames += 1;
    }

    fn latest_payload(&self) -> Option<PluginFramePayload> {
        self.latest.as_ref().map(|latest| latest.payload.clone())
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
    let (mailbox, mut receiver) =
        mpsc::channel::<PluginMessage>(DEBUG_TOGGLE_ACTOR_MAILBOX_CAPACITY);
    let actor_id = DEBUG_TOGGLE_ACTOR_ID.to_string();
    let task_actor_id = actor_id.clone();
    let task_host_sender = host_sender.clone();
    let task = runtime.spawn(async move {
        info!(plugin_actor = %task_actor_id, "plugin actor started");
        let mut spawn_btop_next = false;
        publish_toggle_frame(&task_host_sender, &task_actor_id, spawn_btop_next);
        while let Some(message) = receiver.recv().await {
            info!(
                plugin_actor = %task_actor_id,
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

    PluginActorHandle {
        id: actor_id,
        kind: DEBUG_TOGGLE_ACTOR_KIND.to_string(),
        mailbox,
        enqueued_messages: 0,
        dropped_messages: 0,
        mailbox_capacity: DEBUG_TOGGLE_ACTOR_MAILBOX_CAPACITY,
        swapchains: Vec::new(),
    }
}

fn publish_toggle_frame(
    host_sender: &channel::Sender<PluginHostEvent>,
    actor_id: &str,
    spawn_btop_next: bool,
) {
    if let Err(err) = host_sender.send(PluginHostEvent::AppendSwapchainFrame {
        actor_id: actor_id.to_string(),
        slot: DEFAULT_SWAPCHAIN_SLOT.to_string(),
        frame: PluginFramePayload::ToggleChoice { spawn_btop_next },
    }) {
        warn!(plugin_actor = %actor_id, error = ?err, "plugin actor could not publish swapchain frame");
    }
}
