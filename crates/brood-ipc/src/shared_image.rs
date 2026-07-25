//! Size-sealed shared-memory image resources.

#![allow(
    clippy::cast_ptr_alignment,
    reason = "mmap returns page-aligned addresses, which exceed the header's alignment"
)]

use std::{
    ffi::CString,
    mem::size_of,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    sync::atomic::{AtomicU64, Ordering},
};

use brood_protocol::{FrameColorSpace, FramePixelFormat};
use memmap2::{Mmap, MmapMut, MmapOptions};
use thiserror::Error;

const MAGIC: u64 = 0x4252_4f4f_4449_4d47;
const ABI_VERSION: u32 = 1;

#[repr(C, align(64))]
struct Header {
    magic: u64,
    abi_version: u32,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
    color_space: u32,
    connection_generation: u64,
    byte_len: u64,
    sequence: AtomicU64,
    reserved: [u8; 72],
}

const _: () = assert!(size_of::<Header>() == 128);

/// Immutable description of a packed shared-memory image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedImageMetadata {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: FramePixelFormat,
    pub color_space: FrameColorSpace,
}

impl SharedImageMetadata {
    fn byte_len(self) -> Result<usize, SharedImageError> {
        let len = u64::from(self.stride)
            .checked_mul(u64::from(self.height))
            .ok_or(SharedImageError::InvalidMetadata)?;
        usize::try_from(len).map_err(|_error| SharedImageError::InvalidMetadata)
    }

    fn validate(self) -> Result<(), SharedImageError> {
        let min_stride = self
            .width
            .checked_mul(4)
            .ok_or(SharedImageError::InvalidMetadata)?;
        if self.width == 0 || self.height == 0 || self.stride < min_stride || self.byte_len()? == 0
        {
            return Err(SharedImageError::InvalidMetadata);
        }
        Ok(())
    }
}

/// Failure to create, attach, publish, or read a shared image.
#[derive(Debug, Error)]
pub enum SharedImageError {
    #[error("shared-image system call failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("shared-image metadata is invalid")]
    InvalidMetadata,
    #[error("shared-image mapping is smaller than its declared layout")]
    MappingTooSmall,
    #[error("shared-image ABI header is invalid")]
    InvalidHeader,
    #[error("shared-image connection generation is stale")]
    StaleGeneration,
    #[error("shared-image payload length does not match its resource")]
    PayloadLength,
    #[error("shared-image frame sequence has not been published")]
    Sequence,
}

/// Writable shared image retained by the session server.
#[derive(Debug)]
pub struct SharedImageProducer {
    fd: OwnedFd,
    mapping: MmapMut,
    metadata: SharedImageMetadata,
    byte_len: usize,
}

impl SharedImageProducer {
    /// Create one initialized image resource for a connection generation.
    pub fn create(
        connection_generation: u64,
        metadata: SharedImageMetadata,
    ) -> Result<Self, SharedImageError> {
        metadata.validate()?;
        let byte_len = metadata.byte_len()?;
        let total_len = size_of::<Header>()
            .checked_add(byte_len)
            .ok_or(SharedImageError::InvalidMetadata)?;
        let name = CString::new("brood-shared-image")
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        // SAFETY: `name` is NUL terminated and the flag set is valid.
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful `memfd_create` returns one newly-owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let total_len =
            libc::off_t::try_from(total_len).map_err(|_error| SharedImageError::InvalidMetadata)?;
        // SAFETY: the descriptor is live and `total_len` is a non-negative size.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), total_len) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: this is the unique mapping used to initialize the new file.
        let mut mapping = unsafe { MmapOptions::new().map_mut(&fd)? };
        let header = Header {
            magic: MAGIC,
            abi_version: ABI_VERSION,
            width: metadata.width,
            height: metadata.height,
            stride: metadata.stride,
            pixel_format: metadata.pixel_format.into(),
            color_space: metadata.color_space.into(),
            connection_generation,
            byte_len: u64::try_from(byte_len)
                .map_err(|_error| SharedImageError::InvalidMetadata)?,
            sequence: AtomicU64::new(0),
            reserved: [0; 72],
        };
        // SAFETY: the mapping is suitably aligned and large enough for `Header`
        // followed by the validated payload. It is not otherwise reachable.
        unsafe {
            mapping.as_mut_ptr().cast::<Header>().write(header);
        }
        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: this file was created with `MFD_ALLOW_SEALING`.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            fd,
            mapping,
            metadata,
            byte_len,
        })
    }

    /// Publish complete packed pixels before advancing the visible sequence.
    pub fn publish(&mut self, sequence: u64, pixels: &[u8]) -> Result<(), SharedImageError> {
        if pixels.len() != self.byte_len {
            return Err(SharedImageError::PayloadLength);
        }
        let payload = self
            .mapping
            .get_mut(size_of::<Header>()..)
            .ok_or(SharedImageError::MappingTooSmall)?;
        payload.copy_from_slice(pixels);
        // SAFETY: the constructor initialized a live aligned `Header` at the
        // mapping base, and the mapping remains uniquely owned by this producer.
        let header = unsafe { &*self.mapping.as_ptr().cast::<Header>() };
        header.sequence.store(sequence, Ordering::Release);
        Ok(())
    }

    /// Image metadata fixed for this resource's lifetime.
    #[must_use]
    pub const fn metadata(&self) -> SharedImageMetadata {
        self.metadata
    }
}

impl AsFd for SharedImageProducer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Read-only shared image retained by the presentation client.
#[derive(Debug)]
pub struct SharedImageConsumer {
    mapping: Mmap,
    metadata: SharedImageMetadata,
    byte_len: usize,
}

impl SharedImageConsumer {
    /// Attach to a descriptor registered for one connection generation.
    pub fn map(fd: BorrowedFd<'_>, connection_generation: u64) -> Result<Self, SharedImageError> {
        // SAFETY: the descriptor remains borrowed while `mmap` duplicates its
        // backing reference into the returned mapping.
        let mapping = unsafe { MmapOptions::new().map(&fd)? };
        if mapping.len() < size_of::<Header>() {
            return Err(SharedImageError::MappingTooSmall);
        }
        // SAFETY: the length check covers the aligned fixed header. Header
        // fields are immutable after producer initialization except sequence.
        let header = unsafe { &*mapping.as_ptr().cast::<Header>() };
        if header.magic != MAGIC || header.abi_version != ABI_VERSION {
            return Err(SharedImageError::InvalidHeader);
        }
        if header.connection_generation != connection_generation {
            return Err(SharedImageError::StaleGeneration);
        }
        let metadata = SharedImageMetadata {
            width: header.width,
            height: header.height,
            stride: header.stride,
            pixel_format: header
                .pixel_format
                .try_into()
                .map_err(|_error| SharedImageError::InvalidMetadata)?,
            color_space: header
                .color_space
                .try_into()
                .map_err(|_error| SharedImageError::InvalidMetadata)?,
        };
        metadata.validate()?;
        let byte_len =
            usize::try_from(header.byte_len).map_err(|_error| SharedImageError::InvalidMetadata)?;
        if metadata.byte_len()? != byte_len
            || mapping.len() < size_of::<Header>().saturating_add(byte_len)
        {
            return Err(SharedImageError::MappingTooSmall);
        }
        Ok(Self {
            mapping,
            metadata,
            byte_len,
        })
    }

    /// Borrow pixels for the requested published frame sequence.
    pub fn pixels(&self, sequence: u64) -> Result<&[u8], SharedImageError> {
        // SAFETY: `map` validated the complete fixed header at this address.
        let header = unsafe { &*self.mapping.as_ptr().cast::<Header>() };
        if header.sequence.load(Ordering::Acquire) != sequence {
            return Err(SharedImageError::Sequence);
        }
        self.mapping
            .get(size_of::<Header>()..size_of::<Header>().saturating_add(self.byte_len))
            .ok_or(SharedImageError::MappingTooSmall)
    }

    /// Image metadata fixed for this resource's lifetime.
    #[must_use]
    pub const fn metadata(&self) -> SharedImageMetadata {
        self.metadata
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use brood_protocol::{FrameColorSpace, FramePixelFormat};

    use super::{SharedImageConsumer, SharedImageError, SharedImageMetadata, SharedImageProducer};

    #[test]
    fn published_pixels_are_visible_at_the_matching_sequence() {
        let metadata = SharedImageMetadata {
            width: 2,
            height: 2,
            stride: 8,
            pixel_format: FramePixelFormat::Rgba8,
            color_space: FrameColorSpace::Srgb,
        };
        let producer_result = SharedImageProducer::create(7, metadata);
        assert!(producer_result.is_ok());
        let Some(mut producer) = producer_result.ok() else {
            return;
        };
        let consumer_result = SharedImageConsumer::map(producer.as_fd(), 7);
        assert!(consumer_result.is_ok());
        let Some(consumer) = consumer_result.ok() else {
            return;
        };
        assert!(matches!(
            consumer.pixels(1),
            Err(SharedImageError::Sequence)
        ));
        let pixels = [0x55; 16];
        assert!(producer.publish(1, &pixels).is_ok());
        let visible = consumer.pixels(1);
        assert!(visible.is_ok());
        let Some(visible) = visible.ok() else {
            return;
        };
        assert_eq!(visible, pixels.as_slice());
        assert_eq!(consumer.metadata(), metadata);
    }
}
