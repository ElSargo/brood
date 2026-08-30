//! Private local bootstrap, control, and descriptor-broker protocol.
//!
//! D-Bus is deliberately absent here. This packet protocol exists only to
//! establish the shared-memory transport and move generation-owned descriptors.

use brood_protocol::FrameTransportKind;
use serde::{Deserialize, Serialize};

/// Maximum encoded bootstrap request or response size.
pub const LOCAL_CONTROL_MAX_BYTES: usize = 64 * 1024;
/// Payload bytes carried by one chunk of a large control-command response.
///
/// This leaves bounded headroom for postcard's response envelope while keeping
/// the underlying packet transport at its deliberately small 64-KiB limit.
pub const LOCAL_CONTROL_CHUNK_BYTES: usize = 48 * 1024;
/// Maximum descriptors in one bootstrap response.
pub const LOCAL_CONTROL_MAX_FDS: usize = 16;

/// One private request from a Brood-owned local endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalControlRequest {
    Handshake {
        protocol_version: u32,
    },
    Control {
        argv: Vec<String>,
        json: bool,
    },
    OpenPresentation {
        frame_transports: Vec<FrameTransportKind>,
    },
    DmabufResource {
        generation: u64,
        resource: u64,
    },
    SharedMemoryResource {
        generation: u64,
        resource: u64,
    },
    InstallSessionInitArtifact {
        path: String,
    },
    TakeRealmArtifactRequests,
    CompleteRealmArtifactRequest {
        request: u64,
        success: bool,
        message: String,
    },
}

/// One private local-control response. Descriptors are attached out of band.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalControlResponse {
    pub result: Result<LocalControlValue, String>,
}

/// Successful private local-control values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalControlValue {
    Handshake {
        protocol_version: u32,
        server_pid: u32,
    },
    Presentation {
        generation: u64,
        active_session: u64,
        frame_transport: FrameTransportKind,
    },
    DmabufResource(LocalDmabufResource),
    SharedMemoryResource {
        app_id: String,
    },
    Bool(bool),
    Control(String),
    ControlChunk {
        bytes: Vec<u8>,
        complete: bool,
    },
    RealmArtifactRequests(Vec<(u64, String)>),
}

/// Split a control result without discarding any output bytes.
#[must_use]
pub fn local_control_responses(output: String) -> Vec<LocalControlResponse> {
    if output.len() <= LOCAL_CONTROL_CHUNK_BYTES {
        return vec![LocalControlResponse {
            result: Ok(LocalControlValue::Control(output)),
        }];
    }
    let bytes = output.into_bytes();
    let chunk_count = bytes.len().div_ceil(LOCAL_CONTROL_CHUNK_BYTES);
    bytes
        .chunks(LOCAL_CONTROL_CHUNK_BYTES)
        .enumerate()
        .map(|(index, chunk)| LocalControlResponse {
            result: Ok(LocalControlValue::ControlChunk {
                bytes: chunk.to_vec(),
                complete: index + 1 == chunk_count,
            }),
        })
        .collect()
}

/// Metadata accompanying one DMA-BUF plane descriptor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalDmabufResource {
    pub sequence: u64,
    pub app_id: String,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub color_space: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub y_inverted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_control_responses_are_bounded_and_lossless() {
        let output = "observation 🖥️\n".repeat(20_000);
        let responses = local_control_responses(output.clone());
        assert!(responses.len() > 1);

        let mut reconstructed = Vec::new();
        for (index, response) in responses.iter().enumerate() {
            assert!(
                postcard::to_stdvec(response)
                    .is_ok_and(|encoded| encoded.len() <= LOCAL_CONTROL_MAX_BYTES)
            );
            if let Ok(LocalControlValue::ControlChunk { bytes, complete }) = &response.result {
                reconstructed.extend_from_slice(bytes);
                assert_eq!(*complete, index + 1 == responses.len());
            } else {
                assert!(
                    matches!(&response.result, Ok(LocalControlValue::ControlChunk { .. })),
                    "large response was not chunked"
                );
            }
        }
        assert_eq!(
            String::from_utf8(reconstructed).as_deref(),
            Ok(output.as_str())
        );
    }
}
