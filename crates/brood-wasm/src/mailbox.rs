use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

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
    state: Arc<Mutex<MailboxSenderState>>,
}

#[derive(Debug)]
struct MailboxSenderState {
    sender: mpsc::Sender<MailboxMessage>,
    active: bool,
}

/// Generation-scoped address for one exact legacy actor mailbox.
///
/// The address keeps the bounded sender and generation byte budget but never
/// consults the mutable name registry. Deactivating the registered generation
/// makes every cloned address reject later sends; an old address can therefore
/// never resolve to a same-name replacement.
#[derive(Clone, Debug)]
pub struct MailboxAddress {
    actor_id: ActorId,
    sender: MailboxSender,
    byte_budget: GenerationByteBudget,
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
                sender: MailboxSender {
                    state: Arc::new(Mutex::new(MailboxSenderState {
                        sender,
                        active: true,
                    })),
                },
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
            if let Some(actor_id) = self.names.remove(name)
                && let Some(sender) = self.mailboxes.remove(&actor_id)
            {
                sender.deactivate();
            }
        }
        for registration in registrations {
            self.names.insert(registration.name, registration.actor_id);
            self.mailboxes
                .insert(registration.actor_id, registration.sender);
        }
        Ok(())
    }

    /// Atomically activate one prepared generation and return its exact
    /// generation-scoped address. The address is constructed before the
    /// registry mutation, so successful activation can never publish a name
    /// without returning its matching ingress capability.
    pub fn activate_prepared_with_address(
        &mut self,
        registration: PreparedMailboxRegistration,
        replace_names: &[String],
    ) -> Result<MailboxAddress> {
        self.validate_prepared(std::slice::from_ref(&registration), replace_names)?;
        let address = MailboxAddress {
            actor_id: registration.actor_id,
            sender: registration.sender.clone(),
            byte_budget: self.byte_budget.clone(),
        };
        self.activate_prepared(vec![registration], replace_names)?;
        Ok(address)
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

    pub fn mailbox_address(&self, actor_id: ActorId) -> Result<MailboxAddress> {
        Ok(MailboxAddress {
            actor_id,
            sender: self.mailbox_sender(actor_id)?.clone(),
            byte_budget: self.byte_budget.clone(),
        })
    }

    /// Remove an actor generation from the registry. Any retained mailbox
    /// sender becomes disconnected and late posts cannot reach a replacement
    /// actor which happens to reuse the same public name.
    pub fn unregister_actor(&mut self, name: &str) -> bool {
        let Some(actor_id) = self.names.remove(name) else {
            return false;
        };
        let Some(sender) = self.mailboxes.remove(&actor_id) else {
            return false;
        };
        sender.deactivate();
        true
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
        self.mailboxes.get(&actor_id).map(MailboxSender::is_closed)
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
        self.mailbox_address(recipient)?
            .try_post_with_sender(sender, payload)
    }

    pub async fn post_message(&self, recipient: ActorId, payload: Vec<u8>) -> Result<()> {
        ensure!(
            payload.len() <= MAX_MAILBOX_MESSAGE_BYTES,
            "mailbox message exceeds {MAX_MAILBOX_MESSAGE_BYTES} byte limit"
        );
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
        let sender = {
            let mailbox = self.mailbox_sender(recipient)?;
            let state = mailbox
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ensure!(
                state.active,
                "actor {} mailbox generation is retired",
                recipient.raw()
            );
            state.sender.clone()
        };
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

    fn mailbox_sender(&self, recipient: ActorId) -> Result<&MailboxSender> {
        self.mailboxes
            .get(&recipient)
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

impl MailboxSender {
    fn deactivate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = false;
    }

    fn is_closed(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.active || state.sender.is_closed()
    }
}

impl MailboxAddress {
    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    pub fn try_post(&self, payload: Vec<u8>) -> Result<()> {
        self.try_post_with_sender(None, payload)
    }

    fn try_post_with_sender(&self, sender: Option<ActorId>, payload: Vec<u8>) -> Result<()> {
        self.try_post_with_sender_before_send(sender, payload, || {})
    }

    fn try_post_with_sender_before_send(
        &self,
        sender: Option<ActorId>,
        payload: Vec<u8>,
        before_send: impl FnOnce(),
    ) -> Result<()> {
        ensure!(
            payload.len() <= MAX_MAILBOX_MESSAGE_BYTES,
            "mailbox message exceeds {MAX_MAILBOX_MESSAGE_BYTES} byte limit"
        );
        let byte_permit = self.byte_budget.try_reserve(payload.len())?;
        let state = self
            .sender
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            state.active,
            "actor {} mailbox generation is retired",
            self.actor_id.raw()
        );
        before_send();
        state
            .sender
            .try_send(MailboxMessage {
                sender,
                payload,
                _byte_permit: byte_permit,
            })
            .map_err(|err| anyhow::anyhow!("post message to actor {}: {err}", self.actor_id.raw()))
    }
}

impl Drop for MailboxRegistry {
    fn drop(&mut self) {
        for sender in self.mailboxes.values() {
            sender.deactivate();
        }
    }
}

impl Mailbox {
    pub const fn actor_id(&self) -> ActorId {
        self.actor_id
    }

    /// Number of envelopes available to a nonblocking receiver drain.
    pub fn pending_len(&self) -> usize {
        self.inner.len()
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
    use std::{sync::Barrier, time::Duration};

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

    #[test]
    fn mailbox_address_preserves_fifo_capacity_and_message_byte_bounds() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (registration, mut mailbox) = registry.prepare_actor("layout", 3)?;
        let actor_id = mailbox.actor_id();
        registry.activate_prepared(vec![registration], &[])?;
        let address = registry.mailbox_address(actor_id)?;

        for sequence in 20_u64..23 {
            address.try_post(sequence.to_le_bytes().to_vec())?;
        }
        ensure!(
            address.try_post(vec![24]).is_err(),
            "capacity was not enforced"
        );
        for sequence in 20_u64..23 {
            ensure!(mailbox.try_read_message()? == Some(sequence.to_le_bytes().to_vec()));
        }
        ensure!(
            address
                .try_post(vec![0; MAX_MAILBOX_MESSAGE_BYTES + 1])
                .is_err(),
            "message byte bound was not enforced"
        );
        Ok(())
    }

    #[test]
    fn mailbox_addresses_share_the_generation_byte_budget() -> Result<()> {
        let mut registry = MailboxRegistry {
            mailboxes: BTreeMap::new(),
            names: BTreeMap::new(),
            next_actor_id: FIRST_ACTOR_ID,
            byte_budget: GenerationByteBudget::new(3),
        };
        let (registration, mut mailbox) = registry.prepare_actor("layout", 2)?;
        let address = registry.activate_prepared_with_address(registration, &[])?;
        address.try_post(vec![1, 2])?;
        ensure!(
            address.try_post(vec![3, 4]).is_err(),
            "generation byte budget was not shared by the address"
        );
        ensure!(mailbox.try_read_message()? == Some(vec![1, 2]));
        address.try_post(vec![3, 4])?;
        ensure!(mailbox.try_read_message()? == Some(vec![3, 4]));
        Ok(())
    }

    #[test]
    fn replacing_a_name_retires_old_addresses_without_crossing_generations() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (old_registration, mut old_mailbox) = registry.prepare_actor("layout", 2)?;
        let old_id = old_mailbox.actor_id();
        registry.activate_prepared(vec![old_registration], &[])?;
        let old_address = registry.mailbox_address(old_id)?;

        let (new_registration, mut new_mailbox) = registry.prepare_actor("layout", 2)?;
        let new_id = new_mailbox.actor_id();
        registry.activate_prepared(vec![new_registration], &["layout".to_string()])?;
        let new_address = registry.mailbox_address(new_id)?;

        ensure!(old_address.is_closed());
        ensure!(old_address.try_post(vec![1]).is_err());
        new_address.try_post(vec![2])?;
        ensure!(new_mailbox.try_read_message()? == Some(vec![2]));
        drop(old_address);
        ensure!(old_mailbox.try_read_message().is_err());
        Ok(())
    }

    #[test]
    fn dropping_the_registry_retires_every_cloned_address() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (registration, mut mailbox) = registry.prepare_actor("layout", 2)?;
        let address = registry.activate_prepared_with_address(registration, &[])?;
        drop(registry);

        ensure!(address.is_closed());
        ensure!(address.try_post(vec![1]).is_err());
        drop(address);
        ensure!(mailbox.try_read_message().is_err());
        Ok(())
    }

    #[test]
    fn replacement_cannot_return_while_an_address_send_is_paused_before_enqueue() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (old_registration, mut old_mailbox) = registry.prepare_actor("layout", 2)?;
        let old_address = registry.activate_prepared_with_address(old_registration, &[])?;
        let (new_registration, _new_mailbox) = registry.prepare_actor("layout", 2)?;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let sender = {
            let address = old_address.clone();
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                address.try_post_with_sender_before_send(None, vec![7], || {
                    entered.wait();
                    release.wait();
                })
            })
        };
        entered.wait();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let replacement = std::thread::spawn(move || {
            let _started = started_tx.send(());
            let result =
                registry.activate_prepared_with_address(new_registration, &["layout".to_string()]);
            let _returned = returned_tx.send(result.is_ok());
        });
        started_rx.recv()?;
        ensure!(returned_rx.recv_timeout(Duration::from_millis(10)).is_err());
        release.wait();
        sender
            .join()
            .map_err(|_panic| anyhow::anyhow!("sender panicked"))??;
        ensure!(returned_rx.recv()?);
        replacement
            .join()
            .map_err(|_panic| anyhow::anyhow!("replacement panicked"))?;
        ensure!(old_address.is_closed());
        ensure!(old_address.try_post(vec![8]).is_err());
        ensure!(old_mailbox.try_read_message()? == Some(vec![7]));
        Ok(())
    }

    #[test]
    fn unregister_cannot_return_while_an_address_send_is_paused_before_enqueue() -> Result<()> {
        let mut registry = MailboxRegistry::new();
        let (registration, mut mailbox) = registry.prepare_actor("layout", 2)?;
        let address = registry.activate_prepared_with_address(registration, &[])?;
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let sender = {
            let address = address.clone();
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                address.try_post_with_sender_before_send(None, vec![7], || {
                    entered.wait();
                    release.wait();
                })
            })
        };
        entered.wait();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let unregister = std::thread::spawn(move || {
            let _started = started_tx.send(());
            let removed = registry.unregister_actor("layout");
            let _returned = returned_tx.send(removed);
        });
        started_rx.recv()?;
        ensure!(returned_rx.recv_timeout(Duration::from_millis(10)).is_err());
        release.wait();
        sender
            .join()
            .map_err(|_panic| anyhow::anyhow!("sender panicked"))??;
        ensure!(returned_rx.recv()?);
        unregister
            .join()
            .map_err(|_panic| anyhow::anyhow!("unregister panicked"))?;
        ensure!(address.is_closed());
        ensure!(address.try_post(vec![8]).is_err());
        ensure!(mailbox.try_read_message()? == Some(vec![7]));
        Ok(())
    }
}
