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

/// A mailbox generation which has an identity and bounded channel but is not
/// yet reachable through the public actor namespace.
#[derive(Debug)]
pub struct PreparedMailboxRegistration {
    name: String,
    actor_id: ActorId,
    sender: MailboxSender,
}

#[derive(Clone, Debug)]
struct MailboxSender {
    sender: mpsc::Sender<MailboxMessage>,
}

#[derive(Debug)]
struct MailboxMessage {
    sender: Option<ActorId>,
    payload: Vec<u8>,
    _byte_permit: GenerationBytePermit,
}

#[derive(Debug)]
pub struct MailboxEnvelope {
    pub sender: Option<ActorId>,
    pub payload: Vec<u8>,
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
        let (registration, mailbox) = self.prepare_actor(name, capacity)?;
        self.activate_prepared(vec![registration], &[])?;
        Ok(mailbox)
    }

    /// Allocate and instantiate a mailbox without publishing its name. This
    /// lets a host finish loading a candidate actor before replacing a live
    /// generation.
    pub fn prepare_actor(
        &mut self,
        name: impl Into<String>,
        capacity: usize,
    ) -> Result<(PreparedMailboxRegistration, Mailbox)> {
        ensure!(capacity > 0, "mailbox capacity must be non-zero");
        let name = name.into();
        ensure!(!name.is_empty(), "actor name must not be empty");
        let actor_id = self.allocate_actor_id()?;
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            PreparedMailboxRegistration {
                name,
                actor_id,
                sender: MailboxSender { sender },
            },
            Mailbox {
                inner: receiver,
                actor_id,
            },
        ))
    }

    /// Publish a prepared actor set as one registry transition. Names in
    /// `replace_names` belong to the retiring generation; every other live
    /// name remains protected from replacement.
    pub fn activate_prepared(
        &mut self,
        registrations: Vec<PreparedMailboxRegistration>,
        replace_names: &[String],
    ) -> Result<()> {
        self.validate_prepared(&registrations, replace_names)?;
        let replace_names = replace_names
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        for name in replace_names {
            if let Some(actor_id) = self.names.remove(name) {
                self.mailboxes.remove(&actor_id);
            }
        }
        for registration in registrations {
            self.names.insert(registration.name, registration.actor_id);
            self.mailboxes
                .insert(registration.actor_id, registration.sender);
        }
        Ok(())
    }

    pub fn validate_prepared(
        &self,
        registrations: &[PreparedMailboxRegistration],
        replace_names: &[String],
    ) -> Result<()> {
        let mut candidate_names = std::collections::BTreeSet::new();
        let replace_names = replace_names
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        for registration in registrations {
            ensure!(
                candidate_names.insert(registration.name.as_str()),
                "duplicate prepared actor name {:?}",
                registration.name
            );
            ensure!(
                !self.names.contains_key(&registration.name)
                    || replace_names.contains(registration.name.as_str()),
                "actor name {:?} is already registered",
                registration.name
            );
            ensure!(
                !self.mailboxes.contains_key(&registration.actor_id),
                "prepared actor id {} is already registered",
                registration.actor_id.raw()
            );
        }

        Ok(())
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

    /// Remove `name` only when it still identifies the supplied generation.
    /// A retiring task can therefore never unregister a same-name replacement.
    pub fn unregister_actor_generation(&mut self, name: &str, actor_id: ActorId) -> bool {
        if self.actor_id_for_name(name) != Some(actor_id) {
            return false;
        }
        self.unregister_actor(name)
    }

    pub fn contains_actor(&self, actor_id: ActorId) -> bool {
        self.mailboxes.contains_key(&actor_id)
    }

    /// Report whether a registered generation no longer has a mailbox reader.
    #[must_use]
    pub fn actor_mailbox_is_closed(&self, actor_id: ActorId) -> Option<bool> {
        self.mailboxes
            .get(&actor_id)
            .map(|mailbox| mailbox.sender.is_closed())
    }

    pub fn try_post_named_message(&self, recipient: &str, payload: Vec<u8>) -> Result<()> {
        let actor_id = self
            .actor_id_for_name(recipient)
            .with_context(|| format!("unknown recipient actor name {recipient:?}"))?;
        self.try_post_message(actor_id, payload)
    }

    pub fn try_post_named_message_from(
        &self,
        sender: ActorId,
        recipient: &str,
        payload: Vec<u8>,
    ) -> Result<()> {
        let actor_id = self
            .actor_id_for_name(recipient)
            .with_context(|| format!("unknown recipient actor name {recipient:?}"))?;
        self.try_post_message_from(sender, actor_id, payload)
    }

    pub fn try_post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        self.try_post_message_with_sender(None, recipient, payload)
    }

    pub fn try_post_message_from(
        &self,
        sender: ActorId,
        recipient: ActorId,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.try_post_message_with_sender(Some(sender), recipient, payload)
    }

    fn try_post_message_with_sender(
        &self,
        sender: Option<ActorId>,
        recipient: ActorId,
        payload: Vec<u8>,
    ) -> Result<()> {
        ensure!(
            payload.len() <= MAX_MAILBOX_MESSAGE_BYTES,
            "mailbox message exceeds {MAX_MAILBOX_MESSAGE_BYTES} byte limit"
        );
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
        self.mailbox_sender(recipient)?
            .try_send(MailboxMessage {
                sender,
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
                sender: None,
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
        let Some(envelope) = self.read_envelope().await else {
            bail!("actor {} mailbox is closed", self.actor_id.raw());
        };
        Ok(envelope.payload)
    }

    pub fn try_read_message(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.try_read_envelope()?.map(|envelope| envelope.payload))
    }

    pub async fn read_envelope(&mut self) -> Option<MailboxEnvelope> {
        self.inner.recv().await.map(|message| MailboxEnvelope {
            sender: message.sender,
            payload: message.payload,
        })
    }

    pub fn try_read_envelope(&mut self) -> Result<Option<MailboxEnvelope>> {
        match self.inner.try_recv() {
            Ok(message) => Ok(Some(MailboxEnvelope {
                sender: message.sender,
                payload: message.payload,
            })),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                bail!("actor {} mailbox is closed", self.actor_id.raw())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_distinguishes_a_closed_current_generation_from_backpressure() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (registration, mailbox) = registry.prepare_actor("layout", 1)?;
        let actor_id = mailbox.actor_id();
        registry.activate_prepared(vec![registration], &[])?;

        ensure!(registry.actor_mailbox_is_closed(actor_id) == Some(false));
        registry.try_post_message(actor_id, vec![1])?;
        ensure!(registry.actor_mailbox_is_closed(actor_id) == Some(false));

        drop(mailbox);
        ensure!(registry.actor_mailbox_is_closed(actor_id) == Some(true));
        Ok(())
    }
}
