//! Fixed-layout, zero-parse local presentation IPC.
//!
//! The critical lane is a bounded SPSC ring in a size-sealed memfd. The
//! consumer borrows a typed [`CriticalRecord`] directly from the mapping and
//! releases the slot when the borrow is dropped. No wire decoding or payload
//! copy occurs on the consumer path.

#![cfg(target_os = "linux")]

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

#[cfg(not(target_has_atomic = "64"))]
compile_error!("the critical lane requires lock-free 64-bit atomics");

const MAGIC: u64 = 0x4d41_4e49_4352_4954;
const ABI_VERSION: u32 = 1;
/// Number of records retained by one critical lane.
pub const CRITICAL_CAPACITY: usize = 64;
const CRITICAL_CAPACITY_U32: u32 = 64;

/// Pointer moved to an absolute logical position; arguments are x/y `f64` bits.
pub const INPUT_POINTER_MOTION: u32 = 1;
/// Pointer button changed; arguments are Linux button code and pressed boolean.
pub const INPUT_POINTER_BUTTON: u32 = 2;
/// Keyboard key changed; arguments are xkb keycode and pressed boolean.
pub const INPUT_KEYBOARD_KEY: u32 = 3;
/// Pointer wheel changed; arguments are horizontal/vertical `f64` bits.
pub const INPUT_POINTER_AXIS: u32 = 4;
/// Presentation extent changed; arguments are width and height.
pub const INPUT_PRESENTATION_SIZE: u32 = 5;
/// A registered DMA-BUF resource was committed; argument zero is its resource ID.
pub const FRAME_DMABUF_COMMIT: u32 = 0x100;

/// One directly usable critical-lane record.
///
/// Operation-specific code gives the arguments meaning. The representation is
/// fixed, pointer-free, and identical in both processes.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CriticalRecord {
    pub opcode: u32,
    pub flags: u32,
    pub connection_generation: u64,
    pub sequence: u64,
    pub arguments: [u64; 5],
}

const _: () = assert!(size_of::<CriticalRecord>() == 64);

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
    record: UnsafeCell<CriticalRecord>,
}

impl Slot {
    fn new(sequence: u64) -> Self {
        Self {
            sequence: AtomicU64::new(sequence),
            sequence_padding: [0; 56],
            record: UnsafeCell::new(CriticalRecord::default()),
        }
    }
}

#[repr(C, align(128))]
struct LaneLayout {
    header: Header,
    slots: [Slot; CRITICAL_CAPACITY],
}

impl LaneLayout {
    fn new(connection_generation: u64) -> Self {
        Self {
            header: Header {
                magic: MAGIC,
                abi_version: ABI_VERSION,
                capacity: CRITICAL_CAPACITY_U32,
                connection_generation,
                reserved: [0; 104],
            },
            slots: std::array::from_fn(|index| Slot::new(index as u64)),
        }
    }
}

/// Failure to create or attach a critical lane.
#[derive(Debug, Error)]
pub enum CriticalLaneError {
    #[error("critical lane system call failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("critical lane mapping is smaller than the ABI layout")]
    MappingTooSmall,
    #[error("critical lane ABI header is invalid")]
    InvalidHeader,
    #[error("critical lane connection generation is stale")]
    StaleGeneration,
}

/// Size-sealed memfd containing one initialized critical lane.
pub struct CriticalLaneFile {
    fd: OwnedFd,
}

impl CriticalLaneFile {
    /// Create and initialize a lane for one connection generation.
    pub fn create(connection_generation: u64) -> Result<Self, CriticalLaneError> {
        let name = CString::new("manifold-critical-lane")
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
        // SAFETY: this is the only mapping reachable before this function
        // returns, and the file was just created and sized for `LaneLayout`.
        unsafe {
            mapping
                .pointer
                .as_ptr()
                .write(LaneLayout::new(connection_generation));
        };
        drop(mapping);

        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: `fd` is a memfd created with `MFD_ALLOW_SEALING`.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }
}

impl AsFd for CriticalLaneFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

struct Mapping {
    pointer: NonNull<LaneLayout>,
}

impl Mapping {
    fn map(fd: BorrowedFd<'_>) -> Result<Self, CriticalLaneError> {
        // Validate the backing size before touching the mapping. This prevents
        // a truncated descriptor from faulting during header validation.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` points to writable storage and `fd` is borrowed live.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful `fstat` initialized the value.
        let stat = unsafe { stat.assume_init() };
        let required_size = libc::off_t::try_from(size_of::<LaneLayout>())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        if stat.st_size < required_size {
            return Err(CriticalLaneError::MappingTooSmall);
        }

        // SAFETY: arguments describe a shared read/write mapping of the
        // validated descriptor. The returned pointer is checked against
        // `MAP_FAILED` before conversion.
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
            .ok_or_else(|| std::io::Error::other("mmap unexpectedly returned a null pointer"))?;
        Ok(Self { pointer })
    }

    const fn validate(&self, generation: u64) -> Result<(), CriticalLaneError> {
        // SAFETY: `pointer` references a complete mapping for this owner's
        // lifetime. Header fields are immutable after initialization.
        let header = unsafe { &self.pointer.as_ref().header };
        if header.magic != MAGIC
            || header.abi_version != ABI_VERSION
            || header.capacity != CRITICAL_CAPACITY_U32
        {
            return Err(CriticalLaneError::InvalidHeader);
        }
        if header.connection_generation != generation {
            return Err(CriticalLaneError::StaleGeneration);
        }
        Ok(())
    }

    fn slot(&self, position: u64) -> NonNull<Slot> {
        let index = usize::try_from(position % CRITICAL_CAPACITY as u64).unwrap_or(0);
        // SAFETY: modulo capacity guarantees that `index` addresses a slot in
        // the mapped fixed-size array.
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
        // SAFETY: this owner created the mapping with this exact base and size,
        // and no references survive the endpoint's drop.
        let result = unsafe {
            libc::munmap(
                self.pointer.as_ptr().cast::<libc::c_void>(),
                size_of::<LaneLayout>(),
            )
        };
        debug_assert_eq!(result, 0, "critical lane munmap failed");
    }
}

// The mapping is exclusively owned by one SPSC endpoint. It is movable to its
// worker thread but deliberately does not implement `Sync`.
// SAFETY: ownership of an endpoint and all references into its mapping move
// together, and the SPSC protocol synchronizes access with the other process.
unsafe impl Send for Mapping {}

/// Producer endpoint for one critical SPSC lane.
pub struct CriticalProducer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl CriticalProducer {
    /// Attach a producer to an initialized lane.
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, CriticalLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    /// Reserve the next slot, or return `None` when the bounded lane is full.
    pub fn try_reserve(&mut self) -> Option<CriticalSendSlot<'_>> {
        let slot = self.mapping.slot(self.position);
        // SAFETY: `slot` points into the live mapping. Only the producer reads
        // this sequence while deciding ownership.
        let available = unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } == self.position;
        if available {
            Some(CriticalSendSlot {
                producer: self,
                slot,
            })
        } else {
            None
        }
    }

    /// Copy one already-built record into a reserved slot.
    pub fn try_send(&mut self, record: CriticalRecord) -> Result<(), CriticalRecord> {
        let Some(mut slot) = self.try_reserve() else {
            return Err(record);
        };
        *slot.record_mut() = record;
        slot.commit();
        Ok(())
    }
}

/// Exclusive producer access to one mapped record slot.
pub struct CriticalSendSlot<'a> {
    producer: &'a mut CriticalProducer,
    slot: NonNull<Slot>,
}

impl CriticalSendSlot<'_> {
    /// Borrow the fixed record in place. The caller fills typed fields directly.
    pub fn record_mut(&mut self) -> &mut CriticalRecord {
        // SAFETY: the sequence check gave this sole producer ownership until
        // `commit` publishes the slot with a release store.
        unsafe { &mut *self.slot.as_ref().record.get() }
    }

    /// Publish the initialized record to the consumer.
    pub fn commit(self) {
        let published = self.producer.position.wrapping_add(1);
        // SAFETY: this producer exclusively owns the slot until this release
        // store. Prior record writes become visible to an acquiring consumer.
        unsafe {
            self.slot
                .as_ref()
                .sequence
                .store(published, Ordering::Release);
        };
        self.producer.position = published;
    }
}

/// Consumer endpoint for one critical SPSC lane.
pub struct CriticalConsumer {
    mapping: Mapping,
    position: u64,
    _not_sync: PhantomData<UnsafeCell<()>>,
}

impl CriticalConsumer {
    /// Attach a consumer to an initialized lane.
    pub fn map(fd: BorrowedFd<'_>, generation: u64) -> Result<Self, CriticalLaneError> {
        let mapping = Mapping::map(fd)?;
        mapping.validate(generation)?;
        Ok(Self {
            mapping,
            position: 0,
            _not_sync: PhantomData,
        })
    }

    /// Borrow the next typed record directly from shared memory.
    pub fn try_recv(&mut self) -> Option<CriticalReceived<'_>> {
        let slot = self.mapping.slot(self.position);
        let expected = self.position.wrapping_add(1);
        // SAFETY: `slot` points into the live mapping. The acquire pairs with
        // producer publication before the record is borrowed.
        let ready = unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } == expected;
        if ready {
            Some(CriticalReceived {
                consumer: self,
                slot,
            })
        } else {
            None
        }
    }

    /// Busy-wait until the next record can be borrowed directly.
    ///
    /// This is the minimum-latency active path. Event-loop integration should
    /// spin only for a bounded active window before arming its notification.
    pub fn recv_spinning(&mut self) -> CriticalReceived<'_> {
        loop {
            let slot = self.mapping.slot(self.position);
            let expected = self.position.wrapping_add(1);
            // SAFETY: this is the same acquire and mapped-slot ownership check
            // as `try_recv`; the loop creates no borrow until the slot is ready.
            if unsafe { slot.as_ref().sequence.load(Ordering::Acquire) } == expected {
                return CriticalReceived {
                    consumer: self,
                    slot,
                };
            }
            std::hint::spin_loop();
        }
    }
}

/// Borrow of a record directly in the shared mapping.
pub struct CriticalReceived<'a> {
    consumer: &'a mut CriticalConsumer,
    slot: NonNull<Slot>,
}

impl Deref for CriticalReceived<'_> {
    type Target = CriticalRecord;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the consumer acquired the published sequence and retains sole
        // read ownership until `Drop` releases the slot back to the producer.
        unsafe { &*self.slot.as_ref().record.get() }
    }
}

impl Drop for CriticalReceived<'_> {
    fn drop(&mut self) {
        let released = self
            .consumer
            .position
            .wrapping_add(CRITICAL_CAPACITY as u64);
        // SAFETY: dropping the only consumer borrow ends all reads before this
        // release store returns the slot to the producer.
        unsafe {
            self.slot
                .as_ref()
                .sequence
                .store(released, Ordering::Release);
        };
        self.consumer.position = self.consumer.position.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn typed_record_is_borrowed_from_the_mapping() -> Result<()> {
        let file = CriticalLaneFile::create(7)?;
        let mut producer = CriticalProducer::map(file.as_fd(), 7)?;
        let mut consumer = CriticalConsumer::map(file.as_fd(), 7)?;

        let mut slot = producer
            .try_reserve()
            .ok_or_else(|| std::io::Error::other("full"))?;
        slot.record_mut().opcode = 41;
        slot.record_mut().connection_generation = 7;
        slot.record_mut().sequence = 9;
        if let Some(argument) = slot.record_mut().arguments.first_mut() {
            *argument = 123;
        }
        slot.commit();

        let record = consumer
            .try_recv()
            .ok_or_else(|| std::io::Error::other("empty"))?;
        ensure!(record.opcode == 41);
        ensure!(record.connection_generation == 7);
        ensure!(record.sequence == 9);
        ensure!(record.arguments.first() == Some(&123));
        Ok(())
    }

    #[test]
    fn polling_an_empty_lane_does_not_advance_it() -> Result<()> {
        let file = CriticalLaneFile::create(3)?;
        let mut producer = CriticalProducer::map(file.as_fd(), 3)?;
        let mut consumer = CriticalConsumer::map(file.as_fd(), 3)?;

        for _attempt in 0..1_000 {
            ensure!(consumer.try_recv().is_none());
        }
        ensure!(
            producer
                .try_send(CriticalRecord {
                    sequence: 77,
                    ..CriticalRecord::default()
                })
                .is_ok()
        );
        let record = consumer
            .try_recv()
            .ok_or_else(|| std::io::Error::other("published record was lost"))?;
        ensure!(record.sequence == 77);
        Ok(())
    }

    #[test]
    fn capacity_is_bounded_and_slots_are_reused() -> Result<()> {
        let file = CriticalLaneFile::create(1)?;
        let mut producer = CriticalProducer::map(file.as_fd(), 1)?;
        let mut consumer = CriticalConsumer::map(file.as_fd(), 1)?;

        for sequence in 0..CRITICAL_CAPACITY as u64 {
            ensure!(
                producer
                    .try_send(CriticalRecord {
                        sequence,
                        ..CriticalRecord::default()
                    })
                    .is_ok(),
                "lane filled early"
            );
        }
        ensure!(producer.try_reserve().is_none());
        for sequence in 0..CRITICAL_CAPACITY as u64 {
            let record = consumer
                .try_recv()
                .ok_or_else(|| std::io::Error::other("lane emptied early"))?;
            ensure!(record.sequence == sequence);
        }
        ensure!(
            producer
                .try_send(CriticalRecord {
                    sequence: 100,
                    ..CriticalRecord::default()
                })
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn stale_generation_is_rejected() -> Result<()> {
        let file = CriticalLaneFile::create(8)?;
        ensure!(matches!(
            CriticalConsumer::map(file.as_fd(), 9),
            Err(CriticalLaneError::StaleGeneration)
        ));
        Ok(())
    }
}
