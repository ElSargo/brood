//! Policy provider support.
//!
//! A policy provider is latency-critical synchronous plugin code: key policy,
//! layout policy, map policy, focus preflight, and similar host pre-commit
//! decisions.

use std::{collections::BTreeMap, ops::Range, sync::Arc};

use anyhow::{Context, Result, ensure};

use crate::{api::read_range, mailbox::ActorId};

pub const DEFAULT_PUBLICATION_SLOT: &str = "default";
pub const POLICY_PROVIDER_RESPONSE_OK: i32 = 1;
pub const POLICY_PROVIDER_RESPONSE_ABSTAIN: i32 = 0;
pub const POLICY_PROVIDER_RESPONSE_ERROR: i32 = -1;
const MAX_RETAINED_PUBLICATION_FRAMES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyProviderKind {
    Key,
    Layout,
    Map,
    Focus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyProviderDeclaration {
    pub name: String,
    pub kind: PolicyProviderKind,
    pub export_name: String,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyProviderResponse {
    pub status: i32,
    pub payload_ptr: i32,
    pub payload_len: i32,
    pub reserved: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmPolicyProvider {
    declaration: PolicyProviderDeclaration,
}

#[derive(Debug, Default)]
pub struct PublicationRegistry {
    publications: BTreeMap<PublicationKey, Publication>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationSummary {
    pub actor_id: ActorId,
    pub slot: String,
    pub published_frames: u64,
    pub latest_sequence: Option<u64>,
    pub retained_frames: usize,
    pub latest_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct PublicationFrameLease {
    inner: Arc<FrameNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PublicationKey {
    actor_id: ActorId,
    slot: String,
}

#[derive(Debug, Default)]
struct Publication {
    latest: Option<Arc<FrameNode>>,
    published_frames: u64,
}

#[derive(Debug)]
struct FrameNode {
    sequence: u64,
    payload: Arc<[u8]>,
    previous: Option<Arc<Self>>,
    depth: usize,
}

impl PolicyProviderKind {
    pub fn parse_export_tail(tail: &str) -> Option<(Self, String)> {
        crate::policy_provider_link::parse_policy_provider_export_tail(tail)
    }
}

impl PolicyProviderDeclaration {
    pub fn from_export_name(export_name: &str) -> Option<Self> {
        crate::policy_provider_link::parse_policy_provider_export(export_name)
    }
}

impl PolicyProviderResponse {
    pub const BYTE_LEN: usize = 16;

    pub const fn ok(payload_ptr: i32, payload_len: i32) -> Self {
        Self {
            status: POLICY_PROVIDER_RESPONSE_OK,
            payload_ptr,
            payload_len,
            reserved: 0,
        }
    }

    pub const fn abstain() -> Self {
        Self {
            status: POLICY_PROVIDER_RESPONSE_ABSTAIN,
            payload_ptr: 0,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub const fn error() -> Self {
        Self {
            status: POLICY_PROVIDER_RESPONSE_ERROR,
            payload_ptr: 0,
            payload_len: 0,
            reserved: 0,
        }
    }

    pub fn read_from_memory(memory: &[u8], ptr: i32) -> Result<Self> {
        ensure!(ptr > 0, "policy provider returned a null response pointer");
        let start = usize::try_from(ptr).context("convert policy response pointer")?;
        let end = start
            .checked_add(Self::BYTE_LEN)
            .context("policy response descriptor range overflowed")?;
        let bytes = memory.get(start..end).with_context(|| {
            format!("policy response descriptor range {start}..{end} is outside guest memory")
        })?;
        Ok(Self {
            status: read_i32_le(bytes, 0)?,
            payload_ptr: read_i32_le(bytes, 4)?,
            payload_len: read_i32_le(bytes, 8)?,
            reserved: read_i32_le(bytes, 12)?,
        })
    }
}

impl WasmPolicyProvider {
    pub const fn new(declaration: PolicyProviderDeclaration) -> Self {
        Self { declaration }
    }

    pub const fn declaration(&self) -> &PolicyProviderDeclaration {
        &self.declaration
    }
}

impl PublicationRegistry {
    pub const fn new() -> Self {
        Self {
            publications: BTreeMap::new(),
        }
    }

    pub fn append_default_frame(
        &mut self,
        actor_id: ActorId,
        payload: Vec<u8>,
    ) -> Result<PublicationFrameLease> {
        self.append_frame(actor_id, DEFAULT_PUBLICATION_SLOT, payload)
    }

    pub fn append_frame(
        &mut self,
        actor_id: ActorId,
        slot: &str,
        payload: Vec<u8>,
    ) -> Result<PublicationFrameLease> {
        ensure!(!slot.is_empty(), "publication slot must not be empty");

        let key = PublicationKey::new(actor_id, slot);
        self.publications.entry(key).or_default().append(payload)
    }

    pub fn append_frame_from_range(
        &mut self,
        actor_id: ActorId,
        slot: &str,
        memory: &[u8],
        payload: Range<usize>,
    ) -> Result<PublicationFrameLease> {
        let bytes = read_range(memory, payload, "publication frame")?;
        self.append_frame(actor_id, slot, bytes.to_vec())
    }

    pub fn latest_default_frame(&self, actor_id: ActorId) -> Option<PublicationFrameLease> {
        self.latest_frame(actor_id, DEFAULT_PUBLICATION_SLOT)
    }

    pub fn latest_default_for_provider(
        &self,
        provider: &WasmPolicyProvider,
        actor_id: ActorId,
    ) -> Option<PublicationFrameLease> {
        self.latest_for_provider(provider, actor_id, DEFAULT_PUBLICATION_SLOT)
    }

    pub fn latest_for_provider(
        &self,
        _provider: &WasmPolicyProvider,
        actor_id: ActorId,
        slot: &str,
    ) -> Option<PublicationFrameLease> {
        self.latest_frame(actor_id, slot)
    }

    pub fn latest_frame(&self, actor_id: ActorId, slot: &str) -> Option<PublicationFrameLease> {
        if slot.is_empty() {
            return None;
        }

        self.publications
            .get(&PublicationKey::new(actor_id, slot))
            .and_then(Publication::latest_frame)
    }

    pub fn summaries(&self, actor_id: ActorId) -> Vec<PublicationSummary> {
        self.publications
            .iter()
            .filter(|(key, _publication)| key.actor_id == actor_id)
            .map(|(key, publication)| publication.summary(key))
            .collect()
    }
}

impl PublicationKey {
    fn new(actor_id: ActorId, slot: &str) -> Self {
        Self {
            actor_id,
            slot: slot.to_string(),
        }
    }
}

impl Publication {
    fn append(&mut self, payload: Vec<u8>) -> Result<PublicationFrameLease> {
        let sequence = self
            .published_frames
            .checked_add(1)
            .context("publication frame sequence is exhausted")?;
        let previous = self.latest.as_ref().and_then(|latest| {
            (latest.depth < MAX_RETAINED_PUBLICATION_FRAMES).then(|| Arc::clone(latest))
        });
        let depth = previous.as_ref().map_or(1, |previous| previous.depth + 1);
        let frame = Arc::new(FrameNode {
            sequence,
            payload: Arc::from(payload),
            previous,
            depth,
        });

        self.latest = Some(Arc::clone(&frame));
        self.published_frames = sequence;
        Ok(PublicationFrameLease { inner: frame })
    }

    fn latest_frame(&self) -> Option<PublicationFrameLease> {
        self.latest.as_ref().map(|frame| PublicationFrameLease {
            inner: Arc::clone(frame),
        })
    }

    fn summary(&self, key: &PublicationKey) -> PublicationSummary {
        let latest = self.latest.as_deref();
        PublicationSummary {
            actor_id: key.actor_id,
            slot: key.slot.clone(),
            published_frames: self.published_frames,
            latest_sequence: latest.map(|frame| frame.sequence),
            retained_frames: latest.map_or(0, FrameNode::retained_depth),
            latest_bytes: latest.map_or(0, FrameNode::len),
        }
    }
}

impl PublicationFrameLease {
    pub fn sequence(&self) -> u64 {
        self.inner.sequence
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        self.inner.bytes()
    }

    pub fn copy_into(&self, destination: &mut [u8]) -> Result<usize> {
        let len = self.len();
        let output = destination
            .get_mut(..len)
            .context("destination buffer is too small for publication frame")?;
        output.copy_from_slice(self.bytes());
        Ok(len)
    }

    /// Return a host-memory range for the pinned immutable frame payload.
    ///
    /// The range remains valid only while this lease is alive. A WASM policy provider
    /// binding should copy these bytes into guest memory or expose an explicit
    /// read-only frame handle instead of treating this as a guest pointer.
    pub fn byte_range(&self) -> Result<Range<usize>> {
        self.inner.byte_range()
    }

    pub fn retained_depth(&self) -> usize {
        self.inner.retained_depth()
    }
}

fn read_i32_le(bytes: &[u8], offset: usize) -> Result<i32> {
    let end = offset
        .checked_add(4)
        .context("policy response field range overflowed")?;
    let field = bytes
        .get(offset..end)
        .context("policy response field is outside descriptor")?;
    Ok(i32::from_le_bytes(field.try_into().map_err(|err| {
        anyhow::anyhow!("read policy response field: {err}")
    })?))
}

pub(crate) fn is_valid_declaration_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl FrameNode {
    fn bytes(&self) -> &[u8] {
        &self.payload
    }

    fn len(&self) -> usize {
        self.payload.len()
    }

    fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    fn byte_range(&self) -> Result<Range<usize>> {
        let start = self.payload.as_ptr() as usize;
        let end = start
            .checked_add(self.payload.len())
            .context("publication frame pointer range overflowed")?;
        Ok(start..end)
    }

    fn retained_depth(&self) -> usize {
        let mut depth = 1;
        let mut current = self.previous.as_deref();
        while let Some(frame) = current {
            depth += 1;
            current = frame.previous.as_deref();
        }
        depth
    }
}
