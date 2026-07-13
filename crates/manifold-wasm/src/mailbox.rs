use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::mpsc;

use crate::{
    api::read_range,
    resource_limits::{GenerationByteBudget, GenerationBytePermit},
};

/// Legacy byte-only mailbox ceiling. Hosted actor backends additionally bound
/// sender metadata through [`crate::WasmResourceLimits`].
pub const MAX_MAILBOX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MAILBOX_GENERATION_BYTES: usize = 256 * 1024 * 1024;

const FIRST_ACTOR_ID: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActorId(u64);

impl ActorId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
pub struct MailboxRegistry {
    mailboxes: BTreeMap<ActorId, MailboxSender>,
    names: BTreeMap<String, ActorId>,
    next_actor_id: u64,
    byte_budget: GenerationByteBudget,
}

#[derive(Clone, Debug)]
struct MailboxSender {
    sender: mpsc::Sender<MailboxMessage>,
}

#[derive(Debug)]
struct MailboxMessage {
    payload: Vec<u8>,
    _byte_permit: GenerationBytePermit,
}

#[derive(Debug)]
pub struct Mailbox {
    inner: mpsc::Receiver<MailboxMessage>,
    actor_id: ActorId,
}

impl Default for MailboxRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MailboxRegistry {
    pub fn new() -> Self {
        Self {
            mailboxes: BTreeMap::new(),
            names: BTreeMap::new(),
            next_actor_id: FIRST_ACTOR_ID,
            byte_budget: GenerationByteBudget::new(MAX_MAILBOX_GENERATION_BYTES),
        }
    }

    pub fn register_actor(&mut self, name: impl Into<String>, capacity: usize) -> Result<Mailbox> {
        ensure!(capacity > 0, "mailbox capacity must be non-zero");

        let name = name.into();
        ensure!(!name.is_empty(), "actor name must not be empty");
        ensure!(
            !self.names.contains_key(&name),
            "actor name {name:?} is already registered"
        );

        let actor_id = self.allocate_actor_id()?;
        let (sender, receiver) = mpsc::channel(capacity);

        ensure!(
            self.names.insert(name, actor_id).is_none(),
            "actor name was registered concurrently"
        );
        ensure!(
            self.mailboxes
                .insert(actor_id, MailboxSender { sender })
                .is_none(),
            "actor id {} was registered concurrently",
            actor_id.raw()
        );

        Ok(Mailbox {
            inner: receiver,
            actor_id,
        })
    }

    pub fn actor_id_for_name(&self, name: &str) -> Option<ActorId> {
        self.names.get(name).copied()
    }

    /// Remove an actor generation from the registry. Any retained mailbox
    /// sender becomes disconnected and late posts cannot reach a replacement
    /// actor which happens to reuse the same public name.
    pub fn unregister_actor(&mut self, name: &str) -> bool {
        let Some(actor_id) = self.names.remove(name) else {
            return false;
        };
        self.mailboxes.remove(&actor_id).is_some()
    }

    pub fn contains_actor(&self, actor_id: ActorId) -> bool {
        self.mailboxes.contains_key(&actor_id)
    }

    pub fn try_post_named_message(&self, recipient: &str, payload: Vec<u8>) -> Result<()> {
        let actor_id = self
            .actor_id_for_name(recipient)
            .with_context(|| format!("unknown recipient actor name {recipient:?}"))?;
        self.try_post_message(actor_id, payload)
    }

    pub fn try_post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        ensure!(
            payload.len() <= MAX_MAILBOX_MESSAGE_BYTES,
            "mailbox message exceeds {MAX_MAILBOX_MESSAGE_BYTES} byte limit"
        );
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
        self.mailbox_sender(recipient)?
            .try_send(MailboxMessage {
                payload,
                _byte_permit: byte_permit,
            })
            .map_err(|err| anyhow::anyhow!("post message to actor {}: {err}", recipient.raw()))
    }

    pub async fn post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        ensure!(
            payload.len() <= MAX_MAILBOX_MESSAGE_BYTES,
            "mailbox message exceeds {MAX_MAILBOX_MESSAGE_BYTES} byte limit"
        );
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
        let sender = self.mailbox_sender(recipient)?.clone();
        sender
            .send(MailboxMessage {
                payload,
                _byte_permit: byte_permit,
            })
            .await
            .map_err(|err| anyhow::anyhow!("post message to actor {}: {err}", recipient.raw()))
    }

    pub fn try_post_message_from_range(
        &self,
        recipient: ActorId,
        memory: &[u8],
        payload: std::ops::Range<usize>,
    ) -> Result<()> {
        let bytes = read_range(memory, payload, "payload")?;
        self.try_post_message(recipient, bytes.to_vec())
    }

    fn mailbox_sender(&self, recipient: ActorId) -> Result<&mpsc::Sender<MailboxMessage>> {
        self.mailboxes
            .get(&recipient)
            .map(|mailbox| &mailbox.sender)
            .with_context(|| format!("unknown recipient actor id {}", recipient.raw()))
    }

    fn allocate_actor_id(&mut self) -> Result<ActorId> {
        ensure!(self.next_actor_id != 0, "actor id allocator is exhausted");

        let actor_id = ActorId(self.next_actor_id);
        self.next_actor_id = self
            .next_actor_id
            .checked_add(1)
            .context("actor id allocator is exhausted")?;
        Ok(actor_id)
    }
}

impl Mailbox {
    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    /// Read and consume the next mailbox message.
    ///
    /// The WASM actor import copies this returned byte vector directly into the
    /// host-allocated guest mailbox arena before returning to guest code.
    pub async fn read_message(&mut self) -> Result<Vec<u8>> {
        let Some(message) = self.inner.recv().await else {
            bail!("actor {} mailbox is closed", self.actor_id.raw());
        };
        Ok(message.payload)
    }

    pub fn try_read_message(&mut self) -> Result<Option<Vec<u8>>> {
        match self.inner.try_recv() {
            Ok(message) => Ok(Some(message.payload)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                bail!("actor {} mailbox is closed", self.actor_id.raw())
            }
        }
    }
}
