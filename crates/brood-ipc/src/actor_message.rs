//! Bounded actor mailbox transport between one presentation and session realm.

use std::{
    cell::UnsafeCell,
    ffi::CString,
    marker::PhantomData,
    mem::size_of,
    ops::Deref,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

use brood_protocol::BackendActorRealm;

const MAGIC: u64 = 0x4d41_4e49_4143_544d;
const ABI_VERSION: u32 = 3;
const CAPACITY: usize = 16;
const CAPACITY_U32: u32 = 16;
// Match the reliable actor-message capability advertised by the protocol.
// Sixteen records keep the entire lane bounded to 1 MiB while allowing one
// compact automation observation frame per record.
const RECORD_BYTES: usize = 64 * 1024;
const RECORD_HEADER_BYTES: usize = 40;
/// Maximum combined UTF-8 sender, UTF-8 recipient, and mailbox body bytes.
pub const ACTOR_MESSAGE_BODY_CAPACITY: usize = RECORD_BYTES - RECORD_HEADER_BYTES;

const REALM_SESSION: u32 = 1;
const REALM_DAEMON: u32 = 2;

/// One explicitly backend-realm-scoped actor message.
#[repr(C, align(64))]
pub struct ActorMessageRecord {
    connection_generation: u64,
    sequence: u64,
    session_id: u64,
    sender_len: u32,
    recipient_len: u32,
    body_len: u32,
    realm: u32,
    bytes: [u8; ACTOR_MESSAGE_BODY_CAPACITY],
}

const _: () = assert!(size_of::<ActorMessageRecord>() == RECORD_BYTES);

impl ActorMessageRecord {
    /// Encode one actor message after validating the bounded wire payload.
    #[allow(
        clippy::large_stack_arrays,
        reason = "the fixed-size shared-memory ABI record is immediately moved into a Box"
    )]
    pub fn new(
        connection_generation: u64,
        sequence: u64,
        realm: BackendActorRealm,
        sender: &str,
        recipient: &str,
        body: &[u8],
    ) -> Result<Box<Self>, ActorMessageLaneError> {
        let (realm, session_id) = match realm {
            BackendActorRealm::Daemon => (REALM_DAEMON, 0),
            BackendActorRealm::Session(session_id) if session_id != 0 => {
                (REALM_SESSION, session_id)
            }
            BackendActorRealm::Session(_) => return Err(ActorMessageLaneError::InvalidMessage),
        };
        if sender.is_empty() || recipient.is_empty() {
            return Err(ActorMessageLaneError::InvalidMessage);
        }
        let sender_len = sender.len();
        let recipient_len = recipient.len();
        let total = sender_len
            .checked_add(recipient_len)
            .and_then(|value| value.checked_add(body.len()))
            .ok_or(ActorMessageLaneError::MessageTooLarge)?;
        if total > ACTOR_MESSAGE_BODY_CAPACITY {
            return Err(ActorMessageLaneError::MessageTooLarge);
        }
        let sender_len = u32::try_from(sender_len)
            .map_err(|_overflow| ActorMessageLaneError::MessageTooLarge)?;
        let recipient_len = u32::try_from(recipient_len)
            .map_err(|_overflow| ActorMessageLaneError::MessageTooLarge)?;
        let body_len = u32::try_from(body.len())
            .map_err(|_overflow| ActorMessageLaneError::MessageTooLarge)?;
        let mut bytes = [0; ACTOR_MESSAGE_BODY_CAPACITY];
        let sender_end = sender.len();
        let recipient_end = sender_end + recipient.len();
        let body_end = recipient_end + body.len();
        bytes
            .get_mut(..sender_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?
            .copy_from_slice(sender.as_bytes());
        bytes
            .get_mut(sender_end..recipient_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?
            .copy_from_slice(recipient.as_bytes());
        bytes
            .get_mut(recipient_end..body_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?
            .copy_from_slice(body);
        Ok(Box::new(Self {
            connection_generation,
            sequence,
            session_id,
            sender_len,
            recipient_len,
            body_len,
            realm,
            bytes,
        }))
    }

    #[must_use]
    pub const fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Decode the backend actor realm carried by this message.
    pub const fn realm(&self) -> Result<BackendActorRealm, ActorMessageLaneError> {
        match (self.realm, self.session_id) {
            (REALM_DAEMON, 0) => Ok(BackendActorRealm::Daemon),
            (REALM_SESSION, session_id) if session_id != 0 => {
                Ok(BackendActorRealm::Session(session_id))
            }
            _ => Err(ActorMessageLaneError::InvalidMessage),
        }
    }

    /// Decode the sender metadata.
    pub fn sender(&self) -> Result<&str, ActorMessageLaneError> {
        let (sender, _, _) = self.parts()?;
        std::str::from_utf8(sender).map_err(|_error| ActorMessageLaneError::InvalidMessage)
    }

    /// Decode the logical actor recipient.
    pub fn recipient(&self) -> Result<&str, ActorMessageLaneError> {
        let (_, recipient, _) = self.parts()?;
        std::str::from_utf8(recipient).map_err(|_error| ActorMessageLaneError::InvalidMessage)
    }

    /// Borrow the opaque mailbox body.
    pub fn body(&self) -> Result<&[u8], ActorMessageLaneError> {
        let (_, _, body) = self.parts()?;
        Ok(body)
    }

    fn parts(&self) -> Result<MessageParts<'_>, ActorMessageLaneError> {
        let sender_end = usize::try_from(self.sender_len)
            .map_err(|_overflow| ActorMessageLaneError::InvalidMessage)?;
        let recipient_end = sender_end
            .checked_add(
                usize::try_from(self.recipient_len)
                    .map_err(|_overflow| ActorMessageLaneError::InvalidMessage)?,
            )
            .ok_or(ActorMessageLaneError::InvalidMessage)?;
        let body_end = recipient_end
            .checked_add(
                usize::try_from(self.body_len)
                    .map_err(|_overflow| ActorMessageLaneError::InvalidMessage)?,
            )
            .ok_or(ActorMessageLaneError::InvalidMessage)?;
        let sender = self
            .bytes
            .get(..sender_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?;
        let recipient = self
            .bytes
            .get(sender_end..recipient_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?;
        let body = self
            .bytes
            .get(recipient_end..body_end)
            .ok_or(ActorMessageLaneError::InvalidMessage)?;
        Ok((sender, recipient, body))
    }
}

type MessageParts<'a> = (&'a [u8], &'a [u8], &'a [u8]);

#[repr(C, align(128))]
struct Header {
    magic: u64,
    abi_version: u32,
    capacity: u32,
    connection_generation: u64,
    reserved: [u8; 104],
}

#[repr(C, align(128))]
struct Slot {
    sequence: AtomicU64,
    sequence_padding: [u8; 56],
    record: UnsafeCell<ActorMessageRecord>,
}

impl Slot {
    #[allow(
        clippy::large_stack_arrays,
        reason = "the fixed-size record is part of the statically initialized shared-memory layout"
    )]
    const fn new(sequence: u64) -> Self {
        Self {
            sequence: AtomicU64::new(sequence),
            sequence_padding: [0; 56],
            record: UnsafeCell::new(ActorMessageRecord {
                connection_generation: 0,
                sequence: 0,
                session_id: 0,
                sender_len: 0,
                recipient_len: 0,
                body_len: 0,
                realm: 0,
                bytes: [0; ACTOR_MESSAGE_BODY_CAPACITY],
            }),
        }
    }
}

#[repr(C, align(128))]
struct LaneLayout {
    header: Header,
    slots: [Slot; CAPACITY],
}

/// Failure to create, map, encode, or decode an actor-message lane.
#[derive(Debug, Error)]
pub enum ActorMessageLaneError {
    #[error("actor-message lane system call failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("actor-message mapping is smaller than the ABI layout")]
    MappingTooSmall,
    #[error("actor-message lane ABI header is invalid")]
    InvalidHeader,
    #[error("actor-message lane connection generation is stale")]
    StaleGeneration,
    #[error("actor message exceeds the shared-memory record capacity")]
    MessageTooLarge,
    #[error("actor message is malformed")]
    InvalidMessage,
}

/// Size-sealed memfd containing one initialized actor-message lane.
pub struct ActorMessageLaneFile {
    fd: OwnedFd,
}

impl ActorMessageLaneFile {
    /// Create one lane for a presentation connection generation.
    pub fn create(connection_generation: u64) -> Result<Self, ActorMessageLaneError> {
        let name = CString::new("brood-actor-message-lane")
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        // SAFETY: `name` is a live NUL-terminated C string and flags are valid.
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: a successful `memfd_create` returns a new owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let length = size_of::<LaneLayout>();
        let length_off = libc::off_t::try_from(length)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        // SAFETY: the descriptor is valid and `length_off` is non-negative.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), length_off) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mapping = Mapping::map(fd.as_fd())?;
        // SAFETY: this is the only reachable mapping and it has the exact
        // layout size. Each field is initialized in place before either
        // endpoint is returned, avoiding a large temporary stack layout.
        unsafe {
            let layout = mapping.pointer.as_ptr();
            std::ptr::addr_of_mut!((*layout).header).write(Header {
                magic: MAGIC,
                abi_version: ABI_VERSION,
                capacity: CAPACITY_U32,
                connection_generation,
                reserved: [0; 104],
            });
            let slots = std::ptr::addr_of_mut!((*layout).slots).cast::<Slot>();
            for index in 0..CAPACITY {
                slots.add(index).write(Slot::new(index as u64));
            }
        }
        drop(mapping);
        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: `fd` is a memfd created with `MFD_ALLOW_SEALING`.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }
}

impl AsFd for ActorMessageLaneFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

struct Mapping {
    pointer: NonNull<LaneLayout>,
}

impl Mapping {
    fn map(fd: BorrowedFd<'_>) -> Result<Self, ActorMessageLaneError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` is writable and the descriptor remains borrowed live.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful `fstat` initialized the value.
        let stat = unsafe { stat.assume_init() };
        let required_size = libc::off_t::try_from(size_of::<LaneLayout>())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        if stat.st_size < required_size {
            return Err(ActorMessageLaneError::MappingTooSmall);
        }
        // SAFETY: arguments describe a shared read/write mapping of the
        // validated descriptor; `MAP_FAILED` is checked below.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size_of::<LaneLayout>(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        let pointer = NonNull::new(raw.cast::<LaneLayout>())
            .ok_or_else(|| std::io::Error::other("mmap unexpectedly returned null"))?;
        Ok(Self { pointer })
    }

    const fn validate(&self, generation: u64) -> Result<(), ActorMessageLaneError> {
        // SAFETY: the complete mapping remains live and the header is immutable.
        let header = unsafe { &self.pointer.as_ref().header };
        if header.magic != MAGIC
            || header.abi_version != ABI_VERSION
            || header.capacity != CAPACITY_U32
        {
            return Err(ActorMessageLaneError::InvalidHeader);
        }
        if header.connection_generation != generation {
            return Err(ActorMessageLaneError::StaleGeneration);
        }
        Ok(())
    }

    fn slot(&self, position: u64) -> NonNull<Slot> {
        let index = usize::try_from(position % CAPACITY as u64).unwrap_or(0);
        // SAFETY: modulo capacity keeps `index` within the fixed slot array.
        unsafe {
            NonNull::new_unchecked(
                std::ptr::addr_of_mut!((*self.pointer.as_ptr()).slots)
                    .cast::<Slot>()
                    .add(index),
            )
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: this owner created the mapping with this exact base and size.
        let result = unsafe {
            libc::munmap(
                self.pointer.as_ptr().cast::<libc::c_void>(),
                size_of::<LaneLayout>(),
            )
        };
        debug_assert_eq!(result, 0, "actor-message lane munmap failed");
    }
}

// SAFETY: an endpoint exclusively owns its mapping and the SPSC sequence
// protocol synchronizes access with the endpoint in the other process.
unsafe impl Send for Mapping {}

/// Producer endpoint for one direction of actor messages.
pub struct ActorMessageProducer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl ActorMessageProducer {
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, ActorMessageLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    /// Publish one record, returning it unchanged when the lane is full.
    pub fn try_send(
        &mut self,
        record: Box<ActorMessageRecord>,
    ) -> Result<(), Box<ActorMessageRecord>> {
        let slot = self.mapping.slot(self.position);
        // SAFETY: only this producer reads the slot sequence for ownership.
        let available = unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } == self.position;
        if !available {
            return Err(record);
        }
        // SAFETY: the sequence check grants this producer exclusive write
        // ownership until the release publication below.
        unsafe { std::ptr::copy_nonoverlapping(record.as_ref(), slot.as_ref().record.get(), 1) };
        let published = self.position.wrapping_add(1);
        // SAFETY: the record write is complete before this release store.
        unsafe {
            slot.as_ref().sequence.store(published, Ordering::Release);
        }
        self.position = published;
        Ok(())
    }
}

/// Consumer endpoint for one direction of actor messages.
pub struct ActorMessageConsumer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl ActorMessageConsumer {
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, ActorMessageLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    /// Borrow the next actor message without copying it from shared memory.
    pub fn try_recv(&mut self) -> Option<ActorMessageReceived<'_>> {
        let slot = self.mapping.slot(self.position);
        let expected = self.position.wrapping_add(1);
        // SAFETY: acquire pairs with producer publication before borrowing.
        let ready = unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } == expected;
        if ready {
            Some(ActorMessageReceived {
                consumer: self,
                slot,
            })
        } else {
            None
        }
    }
}

/// Borrow of one actor-message record in the shared mapping.
pub struct ActorMessageReceived<'a> {
    consumer: &'a mut ActorMessageConsumer,
    slot: NonNull<Slot>,
}

impl Deref for ActorMessageReceived<'_> {
    type Target = ActorMessageRecord;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the consumer owns the acquired slot until this borrow drops.
        unsafe { &*self.slot.as_ref().record.get() }
    }
}

impl Drop for ActorMessageReceived<'_> {
    fn drop(&mut self) {
        let released = self.consumer.position.wrapping_add(CAPACITY as u64);
        // SAFETY: all reads finish before returning the slot to the producer.
        unsafe {
            self.slot
                .as_ref()
                .sequence
                .store(released, Ordering::Release);
        }
        self.consumer.position = self.consumer.position.wrapping_add(1);
    }
}
