//! Bounded packet-preserving Unix transport with descriptor passing.

use std::{
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::Path,
    time::Duration,
};

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType, accept_with,
    bind, connect, listen, recvmsg, sendmsg, socket_with,
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

/// One decoded packet and its close-on-exec descriptors.
#[derive(Debug)]
pub struct ReceivedPacket<T> {
    pub message: T,
    pub descriptors: Vec<OwnedFd>,
}

/// A connected or listening bounded `SOCK_SEQPACKET` socket.
#[derive(Debug)]
pub struct PacketTransport<const MAX_BYTES: usize, const MAX_FDS: usize> {
    fd: OwnedFd,
}

impl<const MAX_BYTES: usize, const MAX_FDS: usize> PacketTransport<MAX_BYTES, MAX_FDS> {
    pub fn bind_path(path: &Path, backlog: i32) -> Result<Self, PacketTransportError> {
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        bind(&socket, &rustix::net::SocketAddrUnix::new(path)?)?;
        listen(&socket, backlog)?;
        Ok(Self { fd: socket })
    }

    pub fn accept(&self) -> Result<Self, PacketTransportError> {
        Ok(Self {
            fd: accept_with(&self.fd, SocketFlags::CLOEXEC)?,
        })
    }

    pub fn connect_path(path: &Path) -> Result<Self, PacketTransportError> {
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        connect(&socket, &rustix::net::SocketAddrUnix::new(path)?)?;
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
    ) -> Result<(), PacketTransportError> {
        if descriptors.len() > MAX_FDS {
            return Err(PacketTransportError::TooManyDescriptors {
                received: descriptors.len(),
                maximum: MAX_FDS,
            });
        }
        let bytes = postcard::to_stdvec(message)?;
        if bytes.len() > MAX_BYTES {
            return Err(PacketTransportError::PacketTooLarge {
                received: bytes.len(),
                maximum: MAX_BYTES,
            });
        }
        let mut storage = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        let mut ancillary = SendAncillaryBuffer::new(&mut storage);
        if !descriptors.is_empty() && !ancillary.push(SendAncillaryMessage::ScmRights(descriptors))
        {
            return Err(PacketTransportError::AncillaryBuffer);
        }
        let written = sendmsg(
            &self.fd,
            &[IoSlice::new(&bytes)],
            &mut ancillary,
            SendFlags::NOSIGNAL,
        )?;
        if written != bytes.len() {
            return Err(PacketTransportError::PartialPacket {
                expected: bytes.len(),
                written,
            });
        }
        Ok(())
    }

    pub fn receive<T: DeserializeOwned>(&self) -> Result<ReceivedPacket<T>, PacketTransportError> {
        let mut bytes = vec![0_u8; MAX_BYTES];
        let mut vectors = [IoSliceMut::new(&mut bytes)];
        let mut storage = vec![MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut storage);
        let received = recvmsg(
            &self.fd,
            &mut vectors,
            &mut ancillary,
            RecvFlags::CMSG_CLOEXEC | RecvFlags::TRUNC,
        )?;
        if received.bytes == 0 {
            return Err(PacketTransportError::PeerClosed);
        }
        if received.bytes > MAX_BYTES || received.flags.contains(ReturnFlags::TRUNC) {
            return Err(PacketTransportError::PacketTooLarge {
                received: received.bytes,
                maximum: MAX_BYTES,
            });
        }
        if received.flags.contains(ReturnFlags::CTRUNC) {
            return Err(PacketTransportError::TooManyDescriptors {
                received: MAX_FDS.saturating_add(1),
                maximum: MAX_FDS,
            });
        }
        let mut descriptors = Vec::new();
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                descriptors.extend(rights);
            }
        }
        let encoded = bytes
            .get(..received.bytes)
            .ok_or(PacketTransportError::PacketTooLarge {
                received: received.bytes,
                maximum: MAX_BYTES,
            })?;
        let (message, remaining) = postcard::take_from_bytes(encoded)?;
        if !remaining.is_empty() {
            return Err(PacketTransportError::TrailingBytes {
                received: remaining.len(),
            });
        }
        Ok(ReceivedPacket {
            message,
            descriptors,
        })
    }

    pub fn set_receive_timeout(
        &self,
        timeout: Option<Duration>,
    ) -> Result<(), PacketTransportError> {
        rustix::net::sockopt::set_socket_timeout(
            &self.fd,
            rustix::net::sockopt::Timeout::Recv,
            timeout,
        )?;
        Ok(())
    }

    pub fn set_send_timeout(&self, timeout: Option<Duration>) -> Result<(), PacketTransportError> {
        rustix::net::sockopt::set_socket_timeout(
            &self.fd,
            rustix::net::sockopt::Timeout::Send,
            timeout,
        )?;
        Ok(())
    }
}

impl<const MAX_BYTES: usize, const MAX_FDS: usize> AsFd for PacketTransport<MAX_BYTES, MAX_FDS> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[derive(Debug, Error)]
pub enum PacketTransportError {
    #[error("packet transport failed: {0}")]
    Io(#[from] rustix::io::Errno),
    #[error("packet encoding or decoding failed: {0}")]
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

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Read, os::fd::AsFd, thread};

    use anyhow::{Context, Result, ensure};

    use super::PacketTransport;

    #[test]
    fn preserves_packets_and_passes_descriptors_on_a_persistent_connection() -> Result<()> {
        type Transport = PacketTransport<256, 2>;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("packet.sock");
        let payload_path = directory.path().join("payload");
        std::fs::write(&payload_path, b"descriptor payload")?;
        let listener = Transport::bind_path(&path, 1)?;
        let server = thread::spawn(move || -> Result<()> {
            let connection = listener.accept()?;
            let first = connection.receive::<u64>()?;
            ensure!(first.message == 11);
            ensure!(first.descriptors.is_empty());
            connection.send(&String::from("first"), &[])?;

            let second = connection.receive::<u64>()?;
            ensure!(second.message == 22);
            ensure!(second.descriptors.is_empty());
            let payload = File::open(payload_path)?;
            connection.send(&String::from("second"), &[payload.as_fd()])?;
            Ok(())
        });

        let connection = Transport::connect_path(&path)?;
        connection.send(&11_u64, &[])?;
        let first = connection.receive::<String>()?;
        ensure!(first.message == "first");
        ensure!(first.descriptors.is_empty());

        connection.send(&22_u64, &[])?;
        let mut second = connection.receive::<String>()?;
        ensure!(second.message == "second");
        ensure!(second.descriptors.len() == 1);
        let descriptor = second
            .descriptors
            .pop()
            .context("descriptor response was empty")?;
        let mut payload = String::new();
        File::from(descriptor).read_to_string(&mut payload)?;
        ensure!(payload == "descriptor payload");

        server
            .join()
            .map_err(|_panic| anyhow::anyhow!("packet server thread panicked"))??;
        Ok(())
    }
}
