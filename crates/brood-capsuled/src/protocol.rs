//! Versioned messages exchanged with the capsule spawn broker.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current protocol version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Largest encoded request or response accepted by the broker.
pub const MAX_PACKET_BYTES: usize = 64 * 1024;
/// Largest descriptor set accepted with one packet.
pub const MAX_PACKET_FDS: usize = 16;
/// Largest argument vector accepted in one spawn request.
pub const MAX_ARGUMENTS: usize = 256;
/// Largest environment delta accepted in one spawn request.
pub const MAX_ENVIRONMENT_CHANGES: usize = 512;
/// Largest individual byte string in a spawn request.
pub const MAX_FIELD_BYTES: usize = 16 * 1024;

/// Request envelope carrying explicit compatibility and correlation fields.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct RequestFrame {
    pub version: u16,
    pub request_id: u64,
    pub body: Request,
}

impl RequestFrame {
    #[must_use]
    pub const fn current(request_id: u64, body: Request) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body,
        }
    }

    pub fn validate(&self, descriptor_count: usize) -> Result<(), RequestValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(RequestValidationError::UnsupportedVersion {
                received: self.version,
                supported: PROTOCOL_VERSION,
            });
        }
        if descriptor_count > MAX_PACKET_FDS {
            return Err(RequestValidationError::TooManyDescriptors {
                received: descriptor_count,
                maximum: MAX_PACKET_FDS,
            });
        }
        match &self.body {
            Request::Spawn(request) => request.validate(descriptor_count)?,
            Request::Ping | Request::Signal { .. } | Request::Wait { .. } | Request::Shutdown => {
                if descriptor_count != 0 {
                    return Err(RequestValidationError::UnexpectedDescriptors {
                        received: descriptor_count,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Response envelope paired with one request.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResponseFrame {
    pub version: u16,
    pub request_id: u64,
    pub body: Response,
}

impl ResponseFrame {
    #[must_use]
    pub const fn current(request_id: u64, body: Response) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            body,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Request {
    Ping,
    Spawn(SpawnRequest),
    Signal {
        process_id: u64,
        signal: ProcessSignal,
    },
    Wait {
        process_id: u64,
        nohang: bool,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Response {
    Pong,
    Spawned {
        process_id: u64,
        /// Index of the process pidfd in the attached descriptor list.
        pidfd: u8,
    },
    Running {
        process_id: u64,
    },
    Exited {
        process_id: u64,
        status: ProcessExit,
    },
    Signalled {
        process_id: u64,
    },
    ShuttingDown,
    Failed(ProtocolFailure),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProtocolFailure {
    pub kind: FailureKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum FailureKind {
    InvalidRequest,
    UnsupportedVersion,
    Spawn,
    UnknownProcess,
    Process,
    Internal,
}

/// Unix command data represented without assuming UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpawnRequest {
    pub program: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub cwd: Option<Vec<u8>>,
    pub environment: EnvironmentDelta,
    pub stdin: StdioSource,
    pub stdout: StdioSource,
    pub stderr: StdioSource,
    pub disposition: SpawnDisposition,
    pub metadata: LaunchMetadata,
}

impl SpawnRequest {
    pub fn validate(&self, descriptor_count: usize) -> Result<(), RequestValidationError> {
        validate_required_field("program", &self.program)?;
        if self.arguments.len() > MAX_ARGUMENTS {
            return Err(RequestValidationError::TooManyArguments {
                received: self.arguments.len(),
                maximum: MAX_ARGUMENTS,
            });
        }
        for argument in &self.arguments {
            validate_field("argument", argument)?;
        }
        if let Some(cwd) = &self.cwd {
            validate_required_field("cwd", cwd)?;
        }
        self.environment.validate()?;
        for source in [&self.stdin, &self.stdout, &self.stderr] {
            if let StdioSource::Descriptor(index) = source
                && usize::from(*index) >= descriptor_count
            {
                return Err(RequestValidationError::MissingDescriptor {
                    index: *index,
                    received: descriptor_count,
                });
            }
        }
        for index in 0..descriptor_count {
            let referenced = [&self.stdin, &self.stdout, &self.stderr]
                .iter()
                .any(|source| {
                    matches!(source, StdioSource::Descriptor(received) if usize::from(*received) == index)
                });
            if !referenced {
                return Err(RequestValidationError::UnexpectedDescriptor { index });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct EnvironmentDelta {
    pub clear: bool,
    pub set: Vec<EnvironmentValue>,
    pub remove: Vec<Vec<u8>>,
}

impl EnvironmentDelta {
    fn validate(&self) -> Result<(), RequestValidationError> {
        let received = self.set.len().saturating_add(self.remove.len());
        if received > MAX_ENVIRONMENT_CHANGES {
            return Err(RequestValidationError::TooManyEnvironmentChanges {
                received,
                maximum: MAX_ENVIRONMENT_CHANGES,
            });
        }
        for value in &self.set {
            validate_required_field("environment name", &value.name)?;
            validate_field("environment value", &value.value)?;
        }
        for name in &self.remove {
            validate_required_field("environment name", name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct EnvironmentValue {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum StdioSource {
    Inherit,
    Null,
    Descriptor(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum SpawnDisposition {
    Attached,
    Detached,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LaunchMetadata {
    pub session_id: Option<u64>,
    pub pane_id: Option<u64>,
    pub source: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Kill,
    Terminate,
    User1,
    User2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProcessExit {
    Code(i32),
    Signal(i32),
    Other(i32),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RequestValidationError {
    #[error("protocol version {received} is unsupported; this broker supports {supported}")]
    UnsupportedVersion { received: u16, supported: u16 },
    #[error("{field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("{field} is {received} bytes; the maximum is {maximum}")]
    FieldTooLarge {
        field: &'static str,
        received: usize,
        maximum: usize,
    },
    #[error("spawn request has {received} arguments; the maximum is {maximum}")]
    TooManyArguments { received: usize, maximum: usize },
    #[error("spawn request has {received} environment changes; the maximum is {maximum}")]
    TooManyEnvironmentChanges { received: usize, maximum: usize },
    #[error("packet has {received} descriptors; the maximum is {maximum}")]
    TooManyDescriptors { received: usize, maximum: usize },
    #[error("descriptor index {index} is absent from the {received} received descriptors")]
    MissingDescriptor { index: u8, received: usize },
    #[error("descriptor index {index} is not referenced by the spawn request")]
    UnexpectedDescriptor { index: usize },
    #[error("request type does not accept the {received} attached descriptors")]
    UnexpectedDescriptors { received: usize },
}

const fn validate_required_field(
    field: &'static str,
    value: &[u8],
) -> Result<(), RequestValidationError> {
    if value.is_empty() {
        return Err(RequestValidationError::EmptyField { field });
    }
    validate_field(field, value)
}

const fn validate_field(field: &'static str, value: &[u8]) -> Result<(), RequestValidationError> {
    if value.len() > MAX_FIELD_BYTES {
        return Err(RequestValidationError::FieldTooLarge {
            field,
            received: value.len(),
            maximum: MAX_FIELD_BYTES,
        });
    }
    Ok(())
}
