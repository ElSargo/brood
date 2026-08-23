//! Correlated backend request/response IPC on an isolated shared-memory lane.
//!
//! Backend commands are serialized directly into a small SPSC ring. They do
//! not pay for actor addresses, actor routing validation, or the actor lane's
//! sixteen large observation-sized slots.

use std::{
    cell::UnsafeCell,
    ffi::CString,
    marker::PhantomData,
    mem::size_of,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use brood_protocol::{BackendRequest, BackendResponse};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAGIC: u64 = 0x4252_4f4f_4442_434b;
const ABI_VERSION: u32 = 1;
const CAPACITY: usize = 4;
const CAPACITY_U32: u32 = 4;
const RECORD_BYTES: usize = 64 * 1024;
const RECORD_HEADER_BYTES: usize = 16;
const BODY_CAPACITY: usize = RECORD_BYTES - RECORD_HEADER_BYTES;

/// One value carried by a local backend-control lane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum BackendMessageValue {
    Request(BackendRequest),
    Response(BackendResponse),
}

/// Failure to map, encode, or decode a backend-control lane.
#[derive(Debug, Error)]
pub enum BackendMessageLaneError {
    #[error("backend-control lane system call failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend-control mapping is smaller than the ABI layout")]
    MappingTooSmall,
    #[error("backend-control lane ABI header is invalid")]
    InvalidHeader,
    #[error("backend-control lane connection generation is stale")]
    StaleGeneration,
    #[error("backend-control message encoding failed: {0}")]
    Encode(#[source] postcard::Error),
    #[error("backend-control message decoding failed: {0}")]
    Decode(#[source] postcard::Error),
    #[error("backend-control message is {received} bytes; the maximum is {maximum}")]
    MessageTooLarge { received: usize, maximum: usize },
    #[error("backend-control record is malformed")]
    InvalidRecord,
    #[error("backend-control lane is full")]
    Full,
}

#[repr(C, align(64))]
struct Record {
    sequence: u64,
    body_len: u32,
    reserved: u32,
    body: [u8; BODY_CAPACITY],
}

const _: () = assert!(size_of::<Record>() == RECORD_BYTES);

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
    record: UnsafeCell<Record>,
}

impl Slot {
    #[allow(
        clippy::large_stack_arrays,
        reason = "the fixed-size record is initialized directly into the mmap slot"
    )]
    const fn new(sequence: u64) -> Self {
        Self {
            sequence: AtomicU64::new(sequence),
            sequence_padding: [0; 56],
            record: UnsafeCell::new(Record {
                sequence: 0,
                body_len: 0,
                reserved: 0,
                body: [0; BODY_CAPACITY],
            }),
        }
    }
}

#[repr(C, align(128))]
struct LaneLayout {
    header: Header,
    slots: [Slot; CAPACITY],
}

/// Size-sealed storage for one backend-control direction.
pub struct BackendMessageLaneFile {
    fd: OwnedFd,
}

impl BackendMessageLaneFile {
    pub fn create(connection_generation: u64) -> Result<Self, BackendMessageLaneError> {
        let name = CString::new("brood-backend-message-lane")
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
        // SAFETY: this is the only reachable mapping and every field is
        // initialized before the mapping is shared with either endpoint.
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

impl AsFd for BackendMessageLaneFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

struct Mapping {
    pointer: NonNull<LaneLayout>,
}

impl Mapping {
    fn map(fd: BorrowedFd<'_>) -> Result<Self, BackendMessageLaneError> {
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
            return Err(BackendMessageLaneError::MappingTooSmall);
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

    const fn validate(&self, generation: u64) -> Result<(), BackendMessageLaneError> {
        // SAFETY: the complete mapping remains live and the header is immutable.
        let header = unsafe { &self.pointer.as_ref().header };
        if header.magic != MAGIC
            || header.abi_version != ABI_VERSION
            || header.capacity != CAPACITY_U32
        {
            return Err(BackendMessageLaneError::InvalidHeader);
        }
        if header.connection_generation != generation {
            return Err(BackendMessageLaneError::StaleGeneration);
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
        debug_assert_eq!(result, 0, "backend-control lane munmap failed");
    }
}

// SAFETY: an endpoint exclusively owns its mapping and the SPSC sequence
// protocol synchronizes access with the endpoint in the other process.
unsafe impl Send for Mapping {}

/// Producer for correlated backend request/response values.
pub struct BackendMessageProducer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl BackendMessageProducer {
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, BackendMessageLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    pub fn try_send(&mut self, value: &BackendMessageValue) -> Result<(), BackendMessageLaneError> {
        let bytes = postcard::to_stdvec(value).map_err(BackendMessageLaneError::Encode)?;
        if bytes.len() > BODY_CAPACITY {
            return Err(BackendMessageLaneError::MessageTooLarge {
                received: bytes.len(),
                maximum: BODY_CAPACITY,
            });
        }
        let slot = self.mapping.slot(self.position);
        // SAFETY: only this producer reads the slot sequence for ownership.
        if unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } != self.position {
            return Err(BackendMessageLaneError::Full);
        }
        let body_len = u32::try_from(bytes.len()).map_err(|_overflow| {
            BackendMessageLaneError::MessageTooLarge {
                received: bytes.len(),
                maximum: BODY_CAPACITY,
            }
        })?;
        // SAFETY: the sequence check grants this producer exclusive write
        // ownership until the release publication below. `bytes` was bounded.
        unsafe {
            let record = &mut *slot.as_ref().record.get();
            record.sequence = self.position;
            record.body_len = body_len;
            record.reserved = 0;
            record
                .body
                .get_unchecked_mut(..bytes.len())
                .copy_from_slice(&bytes);
            slot.as_ref()
                .sequence
                .store(self.position.wrapping_add(1), Ordering::Release);
        }
        self.position = self.position.wrapping_add(1);
        Ok(())
    }
}

/// Consumer for correlated backend request/response values.
pub struct BackendMessageConsumer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl BackendMessageConsumer {
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, BackendMessageLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    pub fn try_recv(&mut self) -> Option<Result<BackendMessageReceived, BackendMessageLaneError>> {
        let slot = self.mapping.slot(self.position);
        let expected = self.position.wrapping_add(1);
        // SAFETY: acquire pairs with producer publication before reading.
        if unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } != expected {
            return None;
        }
        // SAFETY: the consumer owns this slot until it publishes the release.
        let result = (|| {
            // SAFETY: the sequence check grants this consumer exclusive read
            // ownership until the release publication below.
            let record = unsafe { &*slot.as_ref().record.get() };
            let length = usize::try_from(record.body_len)
                .map_err(|_overflow| BackendMessageLaneError::InvalidRecord)?;
            let body = record
                .body
                .get(..length)
                .ok_or(BackendMessageLaneError::InvalidRecord)?;
            let value = postcard::from_bytes(body).map_err(BackendMessageLaneError::Decode)?;
            Ok(BackendMessageReceived {
                sequence: record.sequence,
                value,
            })
        })();
        let released = self.position.wrapping_add(CAPACITY as u64);
        // SAFETY: all reads finish before returning the slot to the producer.
        unsafe {
            slot.as_ref().sequence.store(released, Ordering::Release);
        }
        self.position = self.position.wrapping_add(1);
        Some(result)
    }
}

/// Decoded backend-control value detached from its shared-memory slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendMessageReceived {
    pub sequence: u64,
    pub value: BackendMessageValue,
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use anyhow::{Result, ensure};
    use brood_protocol::{BackendOperation, BackendRequest, BackendResponse, BackendResponseValue};

    use super::*;

    #[test]
    fn request_and_response_lanes_are_typed_and_correlated() -> Result<()> {
        let file = BackendMessageLaneFile::create(7)?;
        let mut producer = BackendMessageProducer::map(file.as_fd(), 7)?;
        let mut consumer = BackendMessageConsumer::map(file.as_fd(), 7)?;
        let request = BackendMessageValue::Request(BackendRequest {
            id: 42,
            operation: BackendOperation::Control {
                argv: vec!["status".to_string()],
            },
        });
        producer.try_send(&request)?;
        let received = consumer
            .try_recv()
            .ok_or_else(|| anyhow::anyhow!("request was not published"))??;
        ensure!(received.value == request);

        let response = BackendMessageValue::Response(BackendResponse {
            id: 42,
            result: Ok(BackendResponseValue::Control("OK".to_string())),
        });
        producer.try_send(&response)?;
        let received = consumer
            .try_recv()
            .ok_or_else(|| anyhow::anyhow!("response was not published"))??;
        ensure!(received.value == response);

        for id in 0..CAPACITY as u64 {
            producer.try_send(&BackendMessageValue::Response(BackendResponse {
                id,
                result: Ok(BackendResponseValue::Session(None)),
            }))?;
        }
        ensure!(matches!(
            producer.try_send(&response),
            Err(BackendMessageLaneError::Full)
        ));
        for id in 0..CAPACITY as u64 {
            let received = consumer
                .try_recv()
                .ok_or_else(|| anyhow::anyhow!("queued response {id} was not published"))??;
            ensure!(received.sequence == id + 2);
        }
        producer.try_send(&response)?;
        Ok(())
    }

    #[test]
    fn lane_uses_four_records_instead_of_an_actor_mailbox_mapping() -> Result<()> {
        let file = BackendMessageLaneFile::create(9)?;
        let metadata = rustix::fs::fstat(file.as_fd())?;
        ensure!(metadata.st_size == i64::try_from(size_of::<LaneLayout>())?);
        ensure!(metadata.st_size < 300 * 1024);
        Ok(())
    }
}
