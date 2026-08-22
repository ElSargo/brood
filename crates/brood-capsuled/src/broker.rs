//! Spawn broker shared by capsule PID-1 mode and explicit direct mode.

use std::{
    fs,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rustix::net::{
    AddressFamily, SocketAddrUnix, SocketFlags, SocketType, accept_with, bind, listen, socket_with,
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
};
use thiserror::Error;

use crate::{
    init::{InitError, InitMode, InitProcess, WaitResult},
    protocol::{
        FailureKind, ProtocolFailure, Request, RequestFrame, Response, ResponseFrame, SpawnRequest,
    },
    transport::{ReceivedPacket, SeqPacket, TransportError},
};

pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(3);
const BROKER_TICK: Duration = Duration::from_millis(50);
const BROKER_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PENDING_WAITS: usize = 4_096;

/// Serve one already-connected broker socket.
pub fn serve_connected(
    socket: &SeqPacket,
    mode: InitMode,
    termination_grace: Duration,
) -> Result<(), BrokerError> {
    let mut init = InitProcess::new(mode)?;
    let termination = TerminationSignals::new()?;
    socket.set_send_timeout(Some(BROKER_SEND_TIMEOUT))?;
    let result = serve_loop(socket, &mut init, termination.as_ref());
    let cleanup = init.terminate_all(termination_grace);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Bind a private packet socket, accept one daemon connection, and serve it.
pub fn serve_path(
    socket_path: &Path,
    mode: InitMode,
    termination_grace: Duration,
) -> Result<(), BrokerError> {
    if socket_path.exists() {
        return Err(BrokerError::SocketPathExists(socket_path.to_path_buf()));
    }
    let address = SocketAddrUnix::new(socket_path)?;
    let listener = socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    bind(&listener, &address)?;
    let socket_guard = SocketPathGuard(socket_path.to_path_buf());
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    listen(&listener, 1)?;
    let connection = accept_with(&listener, SocketFlags::CLOEXEC)?;
    let result = serve_connected(
        &SeqPacket::from_owned_fd(connection),
        mode,
        termination_grace,
    );
    drop(socket_guard);
    result
}

fn serve_loop(
    socket: &SeqPacket,
    init: &mut InitProcess,
    termination: Option<&TerminationSignals>,
) -> Result<(), BrokerError> {
    let mut pending_waits = Vec::new();
    loop {
        init.reap_nonblocking()?;
        flush_pending_waits(socket, init, &mut pending_waits)?;
        if termination.is_some_and(TerminationSignals::requested) {
            return Ok(());
        }
        let timeout = Timespec::try_from(BROKER_TICK)
            .map_err(|_error| BrokerError::InvalidPollTimeout(BROKER_TICK))?;
        let mut descriptors = [PollFd::new(socket, PollFlags::IN)];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) | Err(Errno::INTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(BrokerError::Socket(error)),
        }
        let packet: ReceivedPacket<RequestFrame> = match socket.receive() {
            Ok(packet) => packet,
            Err(TransportError::PeerClosed) => return Ok(()),
            Err(TransportError::Io(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR)) => {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let request_id = packet.message.request_id;
        match handle_request(init, packet) {
            HandledRequest::Reply {
                response,
                descriptors,
                shutdown,
            } => {
                send_response(socket, request_id, response, &descriptors)?;
                if shutdown {
                    return Ok(());
                }
            }
            HandledRequest::PendingWait { process_id } => {
                if pending_waits.len() >= MAX_PENDING_WAITS {
                    send_response(
                        socket,
                        request_id,
                        Response::Failed(ProtocolFailure {
                            kind: FailureKind::Process,
                            message: format!(
                                "broker already has the maximum of {MAX_PENDING_WAITS} pending waits"
                            ),
                        }),
                        &[],
                    )?;
                } else {
                    pending_waits.push(PendingWait {
                        request_id,
                        process_id,
                    });
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingWait {
    request_id: u64,
    process_id: u64,
}

enum HandledRequest {
    Reply {
        response: Response,
        descriptors: Vec<OwnedFd>,
        shutdown: bool,
    },
    PendingWait {
        process_id: u64,
    },
}

fn handle_request(init: &mut InitProcess, packet: ReceivedPacket<RequestFrame>) -> HandledRequest {
    if let Err(error) = packet.message.validate(packet.descriptors.len()) {
        let kind = if packet.message.version == crate::protocol::PROTOCOL_VERSION {
            FailureKind::InvalidRequest
        } else {
            FailureKind::UnsupportedVersion
        };
        return failed(kind, error.to_string());
    }

    match packet.message.body {
        Request::Ping => response(Response::Pong),
        Request::Spawn(request) => spawn(init, &request, &packet.descriptors),
        Request::Signal { process_id, signal } => match init.signal(process_id, signal) {
            Ok(()) => response(Response::Signalled { process_id }),
            Err(error) => init_failure(&error),
        },
        Request::Wait { process_id, nohang } => match init.wait(process_id, true) {
            Ok(WaitResult::Running) if !nohang => HandledRequest::PendingWait { process_id },
            Ok(WaitResult::Running) => response(Response::Running { process_id }),
            Ok(WaitResult::Exited(status)) => response(Response::Exited { process_id, status }),
            Err(error) => init_failure(&error),
        },
        Request::Shutdown => HandledRequest::Reply {
            response: Response::ShuttingDown,
            descriptors: Vec::new(),
            shutdown: true,
        },
    }
}

fn spawn(
    init: &mut InitProcess,
    request: &SpawnRequest,
    descriptors: &[OwnedFd],
) -> HandledRequest {
    match init.spawn(request, descriptors) {
        Ok(process) => HandledRequest::Reply {
            response: Response::Spawned {
                process_id: process.process_id,
                pidfd: 0,
            },
            descriptors: vec![process.pidfd],
            shutdown: false,
        },
        Err(error) => init_failure(&error),
    }
}

fn init_failure(error: &InitError) -> HandledRequest {
    response(failure_response(error))
}

fn failure_response(error: &InitError) -> Response {
    let kind = match error {
        InitError::UnknownProcess { .. } => FailureKind::UnknownProcess,
        InitError::Spawn(_) | InitError::Pidfd { .. } => FailureKind::Spawn,
        InitError::MissingDescriptor { .. } => FailureKind::InvalidRequest,
        InitError::NotPidOne { .. }
        | InitError::ProcessIdExhausted
        | InitError::MissingWaitStatus { .. }
        | InitError::MissingRecordedExit { .. } => FailureKind::Internal,
        InitError::TooManyProcesses { .. } | InitError::Process(_) => FailureKind::Process,
    };
    Response::Failed(ProtocolFailure {
        kind,
        message: error.to_string(),
    })
}

const fn response(response: Response) -> HandledRequest {
    HandledRequest::Reply {
        response,
        descriptors: Vec::new(),
        shutdown: false,
    }
}

const fn failed(kind: FailureKind, message: String) -> HandledRequest {
    response(Response::Failed(ProtocolFailure { kind, message }))
}

fn flush_pending_waits(
    socket: &SeqPacket,
    init: &mut InitProcess,
    pending: &mut Vec<PendingWait>,
) -> Result<(), BrokerError> {
    let mut still_pending = Vec::with_capacity(pending.len());
    for wait in pending.drain(..) {
        match init.wait(wait.process_id, true) {
            Ok(WaitResult::Running) => still_pending.push(wait),
            Ok(WaitResult::Exited(status)) => send_response(
                socket,
                wait.request_id,
                Response::Exited {
                    process_id: wait.process_id,
                    status,
                },
                &[],
            )?,
            Err(error) => send_response(socket, wait.request_id, failure_response(&error), &[])?,
        }
    }
    *pending = still_pending;
    Ok(())
}

fn send_response(
    socket: &SeqPacket,
    request_id: u64,
    response: Response,
    descriptors: &[OwnedFd],
) -> Result<(), TransportError> {
    let descriptor_refs: Vec<BorrowedFd<'_>> = descriptors.iter().map(AsFd::as_fd).collect();
    socket.send(
        &ResponseFrame::current(request_id, response),
        &descriptor_refs,
    )
}

struct SocketPathGuard(PathBuf);

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        let _ignored = fs::remove_file(&self.0);
    }
}

struct TerminationSignals {
    requested: Arc<AtomicBool>,
    registrations: Vec<SigId>,
}

impl TerminationSignals {
    fn new() -> Result<Option<Self>, std::io::Error> {
        let requested = Arc::new(AtomicBool::new(false));
        let mut signals = Self {
            requested,
            registrations: Vec::new(),
        };
        for signal in [SIGHUP, SIGINT, SIGTERM] {
            signals.registrations.push(signal_hook::flag::register(
                signal,
                Arc::clone(&signals.requested),
            )?);
        }
        Ok(Some(signals))
    }

    fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            let _removed = signal_hook::low_level::unregister(registration);
        }
    }
}

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("broker transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("broker init failed: {0}")]
    Init(#[from] InitError),
    #[error("broker socket path already exists: {0}")]
    SocketPathExists(PathBuf),
    #[error("broker socket operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("broker socket system call failed: {0}")]
    Socket(#[from] rustix::io::Errno),
    #[error("broker poll interval cannot be represented: {0:?}")]
    InvalidPollTimeout(Duration),
}

#[cfg(test)]
mod tests {
    use std::{io::Read, os::unix::net::UnixStream, thread};

    use super::*;
    use crate::{
        protocol::{
            EnvironmentDelta, LaunchMetadata, ProcessExit, ProcessSignal, Request, RequestFrame,
            Response, ResponseFrame, SpawnDisposition, SpawnRequest, StdioSource,
        },
        transport::ReceivedPacket,
    };

    fn direct_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = SeqPacket::pair()?;
        let (mut output, child_stdout) = UnixStream::pair()?;
        let broker = thread::spawn(move || {
            serve_connected(&server, InitMode::Direct, Duration::from_millis(50))
        });

        client.send(
            &RequestFrame::current(
                1,
                Request::Spawn(SpawnRequest {
                    program: b"/bin/sh".to_vec(),
                    arguments: vec![b"-c".to_vec(), b"printf capsule; exit 37".to_vec()],
                    cwd: None,
                    environment: EnvironmentDelta::default(),
                    stdin: StdioSource::Null,
                    stdout: StdioSource::Descriptor(0),
                    stderr: StdioSource::Null,
                    disposition: SpawnDisposition::Attached,
                    metadata: LaunchMetadata::default(),
                }),
            ),
            &[child_stdout.as_fd()],
        )?;
        drop(child_stdout);
        let spawned: ReceivedPacket<ResponseFrame> = client.receive()?;
        let process_id = match spawned.message.body {
            Response::Spawned { process_id, pidfd } => {
                if pidfd != 0 || spawned.descriptors.len() != 1 {
                    return Err("spawn response did not attach exactly one pidfd".into());
                }
                process_id
            }
            response => return Err(format!("unexpected spawn response: {response:?}").into()),
        };

        let mut child_output = String::new();
        output.read_to_string(&mut child_output)?;
        if child_output != "capsule" {
            return Err(format!("unexpected child output: {child_output:?}").into());
        }

        client.send(
            &RequestFrame::current(
                2,
                Request::Wait {
                    process_id,
                    nohang: false,
                },
            ),
            &[],
        )?;
        let waited: ReceivedPacket<ResponseFrame> = client.receive()?;
        if waited.message.body
            != (Response::Exited {
                process_id,
                status: ProcessExit::Code(37),
            })
        {
            return Err(format!("unexpected wait response: {:?}", waited.message.body).into());
        }

        client.send(&RequestFrame::current(3, Request::Shutdown), &[])?;
        let shutdown: ReceivedPacket<ResponseFrame> = client.receive()?;
        if shutdown.message.body != Response::ShuttingDown {
            return Err(
                format!("unexpected shutdown response: {:?}", shutdown.message.body).into(),
            );
        }
        match broker.join() {
            Ok(result) => result?,
            Err(_panic_payload) => return Err("direct broker thread panicked".into()),
        }
        Ok(())
    }

    #[test]
    fn direct_broker_round_trip_returns_pidfd_and_exit() {
        assert!(direct_round_trip().is_ok());
    }

    fn incompatible_version() -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = SeqPacket::pair()?;
        let broker = thread::spawn(move || {
            serve_connected(&server, InitMode::Direct, Duration::from_millis(50))
        });
        client.send(
            &RequestFrame {
                version: 99,
                request_id: 4,
                body: Request::Ping,
            },
            &[],
        )?;
        let response: ReceivedPacket<ResponseFrame> = client.receive()?;
        if !matches!(
            response.message.body,
            Response::Failed(crate::protocol::ProtocolFailure {
                kind: FailureKind::UnsupportedVersion,
                ..
            })
        ) {
            return Err(format!("unexpected version response: {:?}", response.message.body).into());
        }
        client.send(&RequestFrame::current(5, Request::Shutdown), &[])?;
        let _shutdown: ReceivedPacket<ResponseFrame> = client.receive()?;
        match broker.join() {
            Ok(result) => result?,
            Err(_panic_payload) => return Err("direct broker thread panicked".into()),
        }
        Ok(())
    }

    #[test]
    fn incompatible_version_fails_closed_without_spawning() {
        assert!(incompatible_version().is_ok());
    }

    fn wait_does_not_block_signals() -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = SeqPacket::pair()?;
        let broker = thread::spawn(move || {
            serve_connected(&server, InitMode::Direct, Duration::from_millis(50))
        });
        client.send(
            &RequestFrame::current(
                10,
                Request::Spawn(SpawnRequest {
                    program: b"/bin/sh".to_vec(),
                    arguments: vec![b"-c".to_vec(), b"sleep 30".to_vec()],
                    cwd: None,
                    environment: EnvironmentDelta::default(),
                    stdin: StdioSource::Null,
                    stdout: StdioSource::Null,
                    stderr: StdioSource::Null,
                    disposition: SpawnDisposition::Attached,
                    metadata: LaunchMetadata::default(),
                }),
            ),
            &[],
        )?;
        let spawned: ReceivedPacket<ResponseFrame> = client.receive()?;
        let process_id = match spawned.message.body {
            Response::Spawned { process_id, .. } => process_id,
            response => return Err(format!("unexpected spawn response: {response:?}").into()),
        };

        client.send(
            &RequestFrame::current(
                11,
                Request::Wait {
                    process_id,
                    nohang: false,
                },
            ),
            &[],
        )?;
        client.send(
            &RequestFrame::current(
                12,
                Request::Signal {
                    process_id,
                    signal: ProcessSignal::Terminate,
                },
            ),
            &[],
        )?;

        let signalled: ReceivedPacket<ResponseFrame> = client.receive()?;
        if signalled.message.request_id != 12
            || signalled.message.body != (Response::Signalled { process_id })
        {
            return Err(format!("signal was not serviced during wait: {signalled:?}").into());
        }
        let waited: ReceivedPacket<ResponseFrame> = client.receive()?;
        if waited.message.request_id != 11
            || !matches!(
                waited.message.body,
                Response::Exited {
                    process_id: exited,
                    status: ProcessExit::Signal(_),
                } if exited == process_id
            )
        {
            return Err(format!("deferred wait did not report termination: {waited:?}").into());
        }

        client.send(&RequestFrame::current(13, Request::Shutdown), &[])?;
        let _shutdown: ReceivedPacket<ResponseFrame> = client.receive()?;
        match broker.join() {
            Ok(result) => result?,
            Err(_panic_payload) => return Err("direct broker thread panicked".into()),
        }
        Ok(())
    }

    #[test]
    fn blocking_wait_keeps_signal_lane_live() {
        assert!(wait_does_not_block_signals().is_ok());
    }

    fn deferred_wait_completes_without_followup_request() -> Result<(), Box<dyn std::error::Error>>
    {
        let (client, server) = SeqPacket::pair()?;
        let broker = thread::spawn(move || {
            serve_connected(&server, InitMode::Direct, Duration::from_millis(50))
        });
        client.send(
            &RequestFrame::current(
                20,
                Request::Spawn(SpawnRequest {
                    program: b"/bin/sh".to_vec(),
                    arguments: vec![b"-c".to_vec(), b"sleep 0.1; exit 23".to_vec()],
                    cwd: None,
                    environment: EnvironmentDelta::default(),
                    stdin: StdioSource::Null,
                    stdout: StdioSource::Null,
                    stderr: StdioSource::Null,
                    disposition: SpawnDisposition::Attached,
                    metadata: LaunchMetadata::default(),
                }),
            ),
            &[],
        )?;
        let spawned: ReceivedPacket<ResponseFrame> = client.receive()?;
        let process_id = match spawned.message.body {
            Response::Spawned { process_id, .. } => process_id,
            response => return Err(format!("unexpected spawn response: {response:?}").into()),
        };
        client.send(
            &RequestFrame::current(
                21,
                Request::Wait {
                    process_id,
                    nohang: false,
                },
            ),
            &[],
        )?;
        let waited: ReceivedPacket<ResponseFrame> = client.receive()?;
        if waited.message.body
            != (Response::Exited {
                process_id,
                status: ProcessExit::Code(23),
            })
        {
            return Err(format!("deferred wait did not complete: {waited:?}").into());
        }
        client.send(&RequestFrame::current(22, Request::Shutdown), &[])?;
        let _shutdown: ReceivedPacket<ResponseFrame> = client.receive()?;
        match broker.join() {
            Ok(result) => result?,
            Err(_panic_payload) => return Err("direct broker thread panicked".into()),
        }
        Ok(())
    }

    #[test]
    fn deferred_wait_is_driven_by_broker_tick() {
        assert!(deferred_wait_completes_without_followup_request().is_ok());
    }
}
