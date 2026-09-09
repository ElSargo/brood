//! Versioned service registration, binding, and endpoint observation contracts.
use crate::{ArtifactIdentity, Interface, Realm};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HOST_ENDPOINT: &str = "brood.packages";
pub const GUEST_SENDER: &str = "host:services";
pub const MAX_WIRE_BYTES: usize = 48 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
pub const MAX_INTERFACES: usize = 32;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub interface: String,
    pub supported_versions: Vec<u32>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "brood-service", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    ReportDiagnostic {
        version: u32,
        id: String,
        schema_version: u32,
        value: Option<serde_json::Value>,
        failure: Option<String>,
    },
    Inspect {
        version: u32,
        request_id: u64,
        query: serde_json::Value,
    },
    Register {
        version: u32,
        interfaces: Vec<Registration>,
    },
    Send {
        version: u32,
        binding: String,
        payload: serde_json::Value,
    },
}

impl Request {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err("service request exceeds limit".into());
        }
        let request: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        let version = match &request {
            Self::ReportDiagnostic { version, .. }
            | Self::Register { version, .. }
            | Self::Send { version, .. }
            | Self::Inspect { version, .. } => *version,
        };
        if version != crate::VERSION {
            return Err("unsupported service request version".into());
        }
        Ok(request)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub endpoint: String,
    pub realm: Realm,
    pub scope: String,
    pub actor: String,
    pub generation: u64,
    pub identity: ArtifactIdentity,
    pub provides: Vec<Interface>,
    pub requires: Vec<Interface>,
    pub registered: BTreeMap<String, Vec<u32>>,
}
impl Peer {
    pub fn key(&self) -> String {
        format!(
            "{}/{:?}/{}/{}/{}",
            self.endpoint, self.realm, self.scope, self.actor, self.generation
        )
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Provider,
    Consumer,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Event {
    Inspection {
        request_id: u64,
        response: Box<crate::introspection::Response>,
    },
    Registered {
        interfaces: Vec<String>,
    },
    Bound {
        binding: String,
        role: Role,
        interface: Interface,
        peer: Box<Peer>,
    },
    Unbound {
        binding: String,
        reason: String,
    },
    Message {
        binding: String,
        payload: serde_json::Value,
    },
    Error {
        code: String,
        message: String,
    },
}
impl Event {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err("service event exceeds limit".into());
        }
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub id: String,
    pub interface: Interface,
    pub consumer: Peer,
    pub provider: Peer,
    pub status: String,
}

/// One bounded record at a time. A heartbeat enumerates the current keys so
/// absence is distinguishable from an interrupted observation stream.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Packet {
    Instance {
        version: u32,
        key: String,
        facts: serde_json::Value,
    },
    Observe {
        version: u32,
        peer: Box<Peer>,
        facts: serde_json::Value,
    },
    Heartbeat {
        version: u32,
        endpoint: String,
        revision: u64,
        keys: Vec<String>,
        instance_keys: Vec<String>,
    },
    Offer {
        version: u32,
        binding: Box<Binding>,
    },
    Accept {
        version: u32,
        binding: String,
    },
    Commit {
        version: u32,
        binding: String,
    },
    Revoke {
        version: u32,
        binding: String,
        reason: String,
    },
    Message {
        version: u32,
        binding: String,
        sender: String,
        payload: serde_json::Value,
    },
}
impl Packet {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_WIRE_BYTES {
            return Err("service packet exceeds wire limit".into());
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err("service packet exceeds wire limit".into());
        }
        let packet: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        let version = match &packet {
            Self::Instance { version, .. }
            | Self::Observe { version, .. }
            | Self::Heartbeat { version, .. }
            | Self::Offer { version, .. }
            | Self::Accept { version, .. }
            | Self::Commit { version, .. }
            | Self::Revoke { version, .. }
            | Self::Message { version, .. } => *version,
        };
        if version != crate::VERSION {
            return Err("unsupported service packet version".into());
        }
        Ok(packet)
    }
}

#[cfg(test)]
mod introspection_tests {
    use super::*;
    #[test]
    fn inspection_response_round_trips_through_guest_event() {
        let response = crate::introspection::Response {
            schema: crate::introspection::SCHEMA_ID.into(),
            version: 1,
            endpoint: "host".into(),
            observed_at_unix_ms: 1_700_000_000_000,
            authority: "host-observed".into(),
            collection: Some(crate::introspection::Collection::Instances),
            revision: Some("revision".into()),
            offset: 0,
            next_offset: None,
            total: 0,
            data: vec![],
            error: None,
        };
        let event = Event::Inspection {
            request_id: 42,
            response: Box::new(response),
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        assert_eq!(Event::decode(&bytes).unwrap(), event);
    }
}
