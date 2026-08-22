//! Bounded `SOCK_SEQPACKET` transport with descriptor passing.

use std::{
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::Path,
    time::Duration,
};

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg,
    sendmsg, socket_with, socketpair,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::protocol::{MAX_PACKET_BYTES, MAX_PACKET_FDS};

/// One decoded packet and its close-on-exec descriptors.
#[derive(Debug)]
pub struct ReceivedPacket<T> {
    pub message: T,
    pub descriptors: Vec<OwnedFd>,
}

/// A connected packet-preserving local socket.
#[derive(Debug)]
pub struct SeqPacket {
    fd: OwnedFd,
}

impl SeqPacket {
    pub fn pair() -> Result<(Self, Self), TransportError> {
        let (first, second) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        Ok((Self { fd: first }, Self { fd: second }))
    }

    pub fn connect_path(path: &Path) -> Result<Self, TransportError> {
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        let address = rustix::net::SocketAddrUnix::new(path)?;
        rustix::net::connect(&socket, &address)?;
        Ok(Self { fd: socket })
    }

    #[must_use]
    pub const fn from_owned_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub fn send<T: Serialize>(
        &self,
        message: &T,
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<(), TransportError> {
        if descriptors.len() > MAX_PACKET_FDS {
            return Err(TransportError::TooManyDescriptors {
                received: descriptors.len(),
                maximum: MAX_PACKET_FDS,
            });
        }
        let bytes = postcard::to_stdvec(message)?;
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(TransportError::PacketTooLarge {
                received: bytes.len(),
                maximum: MAX_PACKET_BYTES,
            });
        }

        let mut control_storage =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_PACKET_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut control_storage);
        if !descriptors.is_empty() && !control.push(SendAncillaryMessage::ScmRights(descriptors)) {
            return Err(TransportError::AncillaryBuffer);
        }
        let written = sendmsg(
            &self.fd,
            &[IoSlice::new(&bytes)],
            &mut control,
            SendFlags::NOSIGNAL,
        )?;
        if written != bytes.len() {
            return Err(TransportError::PartialPacket {
                expected: bytes.len(),
                written,
            });
        }
        Ok(())
    }

    pub fn receive<T: DeserializeOwned>(&self) -> Result<ReceivedPacket<T>, TransportError> {
        let mut bytes = vec![0_u8; MAX_PACKET_BYTES];
        let mut vectors = [IoSliceMut::new(&mut bytes)];
        let mut control_storage =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_PACKET_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut control_storage);
        let received = recvmsg(
            &self.fd,
            &mut vectors,
            &mut control,
            RecvFlags::CMSG_CLOEXEC | RecvFlags::TRUNC,
        )?;
        if received.bytes == 0 {
            return Err(TransportError::PeerClosed);
        }
        if received.bytes > MAX_PACKET_BYTES || received.flags.contains(ReturnFlags::TRUNC) {
            return Err(TransportError::PacketTooLarge {
                received: received.bytes,
                maximum: MAX_PACKET_BYTES,
            });
        }
        if received.flags.contains(ReturnFlags::CTRUNC) {
            return Err(TransportError::TooManyDescriptors {
                received: MAX_PACKET_FDS.saturating_add(1),
                maximum: MAX_PACKET_FDS,
            });
        }

        let mut descriptors = Vec::new();
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                descriptors.extend(rights);
            }
        }
        if descriptors.len() > MAX_PACKET_FDS {
            return Err(TransportError::TooManyDescriptors {
                received: descriptors.len(),
                maximum: MAX_PACKET_FDS,
            });
        }

        let encoded = bytes
            .get(..received.bytes)
            .ok_or(TransportError::PacketTooLarge {
                received: received.bytes,
                maximum: MAX_PACKET_BYTES,
            })?;
        let (message, remaining) = postcard::take_from_bytes(encoded)?;
        if !remaining.is_empty() {
            return Err(TransportError::TrailingBytes {
                received: remaining.len(),
            });
        }
        Ok(ReceivedPacket {
            message,
            descriptors,
        })
    }

    pub fn set_receive_timeout(&self, timeout: Option<Duration>) -> Result<(), TransportError> {
        rustix::net::sockopt::set_socket_timeout(
            &self.fd,
            rustix::net::sockopt::Timeout::Recv,
            timeout,
        )?;
        Ok(())
    }

    pub fn set_send_timeout(&self, timeout: Option<Duration>) -> Result<(), TransportError> {
        rustix::net::sockopt::set_socket_timeout(
            &self.fd,
            rustix::net::sockopt::Timeout::Send,
            timeout,
        )?;
        Ok(())
    }
}

impl AsFd for SeqPacket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("packet transport failed: {0}")]
    Io(#[from] rustix::io::Errno),
    #[error("Postcard packet encoding or decoding failed: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("packet is {received} bytes; the maximum is {maximum}")]
    PacketTooLarge { received: usize, maximum: usize },
    #[error("packet has {received} descriptors; the maximum is {maximum}")]
    TooManyDescriptors { received: usize, maximum: usize },
    #[error("packet contained {received} trailing bytes")]
    TrailingBytes { received: usize },
    #[error("ancillary descriptor buffer rejected a bounded descriptor set")]
    AncillaryBuffer,
    #[error("packet send wrote {written} of {expected} bytes")]
    PartialPacket { expected: usize, written: usize },
    #[error("packet peer closed")]
    PeerClosed,
}
