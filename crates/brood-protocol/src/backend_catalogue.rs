//! Versioned actor protocol for the bundled backend/session catalogue UI.
//!
//! These messages cross the client-host to guest-actor boundary. They are not
//! transport commands and do not grant authority: the client remains
//! responsible for deciding which backend operations a guest may request.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{BackendId, SessionIsolationStatus};

/// Current version of the bundled backend catalogue actor protocol.
pub const BACKEND_CATALOGUE_PROTOCOL_VERSION: u32 = 1;

/// Stable route for the bundled catalogue actor.
pub const BACKEND_CATALOGUE_ACTOR_ID: &str = "session-switcher";

/// Sender identity used by the client host for catalogue notifications.
pub const BACKEND_CATALOGUE_CLIENT_SENDER: &str = "client";

/// Defensive limits within the already bounded actor-message envelope.
pub const MAX_BACKEND_CATALOGUE_BACKENDS: usize = 128;
pub const MAX_BACKEND_CATALOGUE_SESSIONS: usize = 2_048;
pub const MAX_BACKEND_CATALOGUE_LABEL_BYTES: usize = 256;
pub const MAX_BACKEND_CATALOGUE_NAME_BYTES: usize = 512;
pub const MAX_BACKEND_CATALOGUE_PATH_BYTES: usize = 4_096;
pub const MAX_BACKEND_CATALOGUE_PROJECT_ID_BYTES: usize = 512;
pub const MAX_BACKEND_CATALOGUE_BRANCH_BYTES: usize = 1_024;
pub const MAX_BACKEND_CONNECTION_ERROR_BYTES: usize = 1_024;
pub const MAX_BACKEND_CAPSULE_FAILURE_BYTES: usize = 1_024;

/// One backend and its latest session catalogue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendCatalogueEntry {
    pub backend: BackendId,
    pub label: String,
    pub sessions: Vec<BackendSessionEntry>,
    /// Presentation catalogue revision this entry was built from.
    ///
    /// `0` means the publisher did not stamp a revision (legacy hosts). Newer
    /// clients fill this from `SessionSnapshot.revision` so consumers can reject
    /// stale non-empty catalogues.
    #[serde(default)]
    pub snapshot_revision: u64,
}

/// Session metadata rendered by the bundled session switcher.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendSessionEntry {
    pub id: u64,
    pub name: String,
    pub active: bool,
    pub windows: u64,
    pub working_directory: Option<String>,
    pub project_id: String,
    pub project_name: String,
    pub branch: Option<String>,
    pub worktree: bool,
    pub git: bool,
    #[serde(default)]
    pub isolation: SessionIsolationStatus,
}

/// Client-host notifications consumed by the bundled session switcher.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum BackendCatalogueMessage {
    #[serde(rename = "backend-connected")]
    Connected { version: u32, backend: BackendId },
    #[serde(rename = "backend-connection-failed")]
    ConnectionFailed {
        version: u32,
        message: String,
        /// Switcher deploy `pending` / host request id that initiated this attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<u64>,
    },
    #[serde(rename = "backend-sessions")]
    Sessions {
        version: u32,
        can_create_local: bool,
        can_connect_remote: bool,
        #[serde(default)]
        can_create_container: bool,
        backends: Vec<BackendCatalogueEntry>,
    },
}

impl BackendCatalogueMessage {
    #[must_use]
    pub const fn connected(backend: BackendId) -> Self {
        Self::Connected {
            version: BACKEND_CATALOGUE_PROTOCOL_VERSION,
            backend,
        }
    }

    #[must_use]
    pub fn connection_failed(message: impl Into<String>) -> Self {
        Self::connection_failed_for_request(None, message)
    }

    #[must_use]
    pub fn connection_failed_for_request(
        request: Option<u64>,
        message: impl Into<String>,
    ) -> Self {
        Self::ConnectionFailed {
            version: BACKEND_CATALOGUE_PROTOCOL_VERSION,
            message: message.into(),
            request,
        }
    }

    #[must_use]
    pub const fn sessions(
        can_create_local: bool,
        can_connect_remote: bool,
        can_create_container: bool,
        backends: Vec<BackendCatalogueEntry>,
    ) -> Self {
        Self::Sessions {
            version: BACKEND_CATALOGUE_PROTOCOL_VERSION,
            can_create_local,
            can_connect_remote,
            can_create_container,
            backends,
        }
    }

    /// Validate the version and bounded guest-facing catalogue fields.
    pub fn validate(&self) -> Result<(), BackendCatalogueValidationError> {
        let version = match self {
            Self::Connected { version, .. }
            | Self::ConnectionFailed { version, .. }
            | Self::Sessions { version, .. } => *version,
        };
        if version != BACKEND_CATALOGUE_PROTOCOL_VERSION {
            return Err(BackendCatalogueValidationError::Version(version));
        }

        match self {
            Self::Connected { .. } => Ok(()),
            Self::ConnectionFailed { message, .. } => validate_text(
                message,
                MAX_BACKEND_CONNECTION_ERROR_BYTES,
                BackendCatalogueTextField::ConnectionError,
            ),
            Self::Sessions { backends, .. } => {
                if backends.len() > MAX_BACKEND_CATALOGUE_BACKENDS {
                    return Err(BackendCatalogueValidationError::Backends(backends.len()));
                }
                let session_count = backends
                    .iter()
                    .try_fold(0_usize, |total, backend| {
                        total.checked_add(backend.sessions.len())
                    })
                    .unwrap_or(usize::MAX);
                if session_count > MAX_BACKEND_CATALOGUE_SESSIONS {
                    return Err(BackendCatalogueValidationError::Sessions(session_count));
                }
                for backend in backends {
                    validate_text(
                        &backend.label,
                        MAX_BACKEND_CATALOGUE_LABEL_BYTES,
                        BackendCatalogueTextField::BackendLabel,
                    )?;
                    for session in &backend.sessions {
                        validate_text(
                            &session.name,
                            MAX_BACKEND_CATALOGUE_NAME_BYTES,
                            BackendCatalogueTextField::SessionName,
                        )?;
                        if let Some(path) = &session.working_directory {
                            validate_text(
                                path,
                                MAX_BACKEND_CATALOGUE_PATH_BYTES,
                                BackendCatalogueTextField::WorkingDirectory,
                            )?;
                        }
                        validate_text(
                            &session.project_id,
                            MAX_BACKEND_CATALOGUE_PROJECT_ID_BYTES,
                            BackendCatalogueTextField::ProjectId,
                        )?;
                        validate_text(
                            &session.project_name,
                            MAX_BACKEND_CATALOGUE_NAME_BYTES,
                            BackendCatalogueTextField::ProjectName,
                        )?;
                        if let Some(branch) = &session.branch {
                            validate_text(
                                branch,
                                MAX_BACKEND_CATALOGUE_BRANCH_BYTES,
                                BackendCatalogueTextField::Branch,
                            )?;
                        }
                        if let Some(failure) = session
                            .isolation
                            .capsule
                            .as_ref()
                            .and_then(|capsule| capsule.failure.as_ref())
                        {
                            validate_text(
                                &failure.message,
                                MAX_BACKEND_CAPSULE_FAILURE_BYTES,
                                BackendCatalogueTextField::CapsuleFailure,
                            )?;
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

const fn validate_text(
    value: &str,
    maximum: usize,
    field: BackendCatalogueTextField,
) -> Result<(), BackendCatalogueValidationError> {
    if value.len() > maximum {
        return Err(BackendCatalogueValidationError::Text {
            field,
            bytes: value.len(),
        });
    }
    Ok(())
}

/// Text field rejected for exceeding its protocol bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendCatalogueTextField {
    BackendLabel,
    SessionName,
    WorkingDirectory,
    ProjectId,
    ProjectName,
    Branch,
    ConnectionError,
    CapsuleFailure,
}

/// Invalid bundled backend catalogue message.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BackendCatalogueValidationError {
    #[error("unsupported backend catalogue protocol version {0}")]
    Version(u32),
    #[error("backend catalogue contains {0} backends, exceeding its bound")]
    Backends(usize),
    #[error("backend catalogue contains {0} sessions, exceeding its bound")]
    Sessions(usize),
    #[error("backend catalogue {field:?} contains {bytes} bytes, exceeding its bound")]
    Text {
        field: BackendCatalogueTextField,
        bytes: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        BACKEND_CATALOGUE_PROTOCOL_VERSION, BackendCatalogueEntry, BackendCatalogueMessage,
        BackendCatalogueValidationError, BackendSessionEntry, MAX_BACKEND_CATALOGUE_NAME_BYTES,
    };
    use crate::{BackendId, SessionIsolationStatus};

    fn session() -> BackendSessionEntry {
        BackendSessionEntry {
            id: 7,
            name: "shell".to_string(),
            active: true,
            windows: 2,
            working_directory: Some("/workspace".to_string()),
            project_id: "project".to_string(),
            project_name: "Project".to_string(),
            branch: Some("main".to_string()),
            worktree: false,
            git: true,
            isolation: SessionIsolationStatus::default(),
        }
    }

    #[test]
    fn typed_messages_preserve_the_bundled_wire_discriminants() {
        let connected = BackendCatalogueMessage::connected(BackendId(3));
        let encoded = serde_json::to_string(&connected);
        assert!(encoded.is_ok());
        let json = encoded.unwrap_or_default();
        assert_eq!(
            json,
            format!(
                "{{\"kind\":\"backend-connected\",\"version\":{BACKEND_CATALOGUE_PROTOCOL_VERSION},\"backend\":3}}"
            )
        );
        assert!(matches!(
            serde_json::from_str::<BackendCatalogueMessage>(&json),
            Ok(decoded) if decoded == connected
        ));
    }

    #[test]
    fn older_catalogues_default_container_creation_to_unavailable() {
        let json = format!(
            "{{\"kind\":\"backend-sessions\",\"version\":{BACKEND_CATALOGUE_PROTOCOL_VERSION},\"can_create_local\":true,\"can_connect_remote\":true,\"backends\":[]}}"
        );
        let decoded = serde_json::from_str::<BackendCatalogueMessage>(&json);
        assert!(matches!(
            decoded,
            Ok(BackendCatalogueMessage::Sessions {
                can_create_container: false,
                ..
            })
        ));
    }

    #[test]
    fn older_catalogue_entries_default_snapshot_revision_to_zero() {
        let json = format!(
            "{{\"kind\":\"backend-sessions\",\"version\":{BACKEND_CATALOGUE_PROTOCOL_VERSION},\"can_create_local\":true,\"can_connect_remote\":false,\"backends\":[{{\"backend\":1,\"label\":\"Local\",\"sessions\":[]}}]}}"
        );
        let decoded = serde_json::from_str::<BackendCatalogueMessage>(&json);
        assert!(matches!(
            decoded,
            Ok(BackendCatalogueMessage::Sessions {
                backends,
                ..
            }) if backends.len() == 1 && backends[0].snapshot_revision == 0
        ));
    }

    #[test]
    fn older_session_entries_default_to_direct_without_volume_or_capsule() {
        let json = r#"{
            "id":7,
            "name":"shell",
            "active":true,
            "windows":2,
            "working_directory":"/workspace",
            "project_id":"project",
            "project_name":"Project",
            "branch":"main",
            "worktree":false,
            "git":true
        }"#;
        let decoded = serde_json::from_str::<BackendSessionEntry>(json);
        assert!(matches!(
            decoded,
            Ok(BackendSessionEntry { isolation, .. }) if isolation == SessionIsolationStatus::default()
        ));
    }

    #[test]
    fn connection_failed_omits_request_by_default_for_wire_compat() {
        let notification = BackendCatalogueMessage::connection_failed("deploy A failed");
        let value = serde_json::to_value(&notification).expect("encode");
        assert!(value.get("request").is_none());
        assert!(matches!(
            notification,
            BackendCatalogueMessage::ConnectionFailed { request: None, .. }
        ));
    }

    #[test]
    fn connection_failed_round_trips_optional_request() {
        let notification =
            BackendCatalogueMessage::connection_failed_for_request(Some(17), "deploy B failed");
        let encoded = serde_json::to_vec(&notification).expect("encode");
        let decoded: BackendCatalogueMessage =
            serde_json::from_slice(&encoded).expect("decode");
        assert!(matches!(
            decoded,
            BackendCatalogueMessage::ConnectionFailed {
                request: Some(17),
                ..
            }
        ));
    }


    #[test]
    fn catalogue_validation_rejects_version_and_oversized_metadata() {
        let unsupported = BackendCatalogueMessage::Connected {
            version: BACKEND_CATALOGUE_PROTOCOL_VERSION + 1,
            backend: BackendId(1),
        };
        assert_eq!(
            unsupported.validate(),
            Err(BackendCatalogueValidationError::Version(
                BACKEND_CATALOGUE_PROTOCOL_VERSION + 1
            ))
        );

        let mut oversized = session();
        oversized.name = "x".repeat(MAX_BACKEND_CATALOGUE_NAME_BYTES + 1);
        let catalogue = BackendCatalogueMessage::sessions(
            true,
            true,
            false,
            vec![BackendCatalogueEntry {
                backend: BackendId(1),
                label: "Local".to_string(),
                sessions: vec![oversized],
                snapshot_revision: 0,
            }],
        );
        assert!(matches!(
            catalogue.validate(),
            Err(BackendCatalogueValidationError::Text { .. })
        ));
    }
}
